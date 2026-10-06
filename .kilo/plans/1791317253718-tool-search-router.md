# Tool Search + BM25 Pre-selection Router

Date: 2026-10-06
Status: draft (validated by user)

## Goal

Replace the "every agent is its own OpenAI tool" model with one
user-facing tool, `search_tools`, that the LLM uses to discover the
tools it actually needs. Add an in-process BM25 router that
pre-selects a small set of likely tools per user turn so trivial
queries (weather, calculate, datetime) still go through in one round
without forcing the model to call `search_tools` first.

`search_tools` follows the Anthropic-style "tool search" pattern:
the LLM calls it, receives matching tool schemas, then invokes the
discovered tool by name as a normal `tool_calls` entry on the next
round.

## Design

### Surface area

What the LLM sees in `tools=[]` at the start of every chat completion
round:

```
[search_tools] + pre_selected(top_k_naming) + already_discovered(session)
```

- `search_tools` is always present.
- `pre_selected` comes from the BM25 router scored against the
  user's most recent message (default top 5).
- `already_discovered` is the union of tool names the LLM has
  invoked earlier in this `chat_session_id`.

What the tool loop accepts as a dispatchable name: any agent that
exists in the registry, **not** just the pre-selected set. That
matches the Anthropic model — once the LLM has the schema from
`search_tools`, it can call the tool by name on the next round even
if it wasn't pre-selected.

### New agent: `search_tools`

`crates/nagent-agents/src/agents/tool_search.rs`. Implements the
existing `Agent` trait so the dispatch path stays generic.

- `name()` → `"search_tools"`.
- `description()` → short prose telling the model to call it for
  "what tools do I have?" and for discovery when unsure.
- `parameters_schema()` →
  ```json
  {
    "type": "object",
    "properties": {
      "query": { "type": "string", "description": "…" },
      "top_k": { "type": "integer", "minimum": 1, "maximum": 20,
                 "default": 5 }
    },
    "required": ["query"]
  }
  ```
- `invoke(ctx, args)`:
  1. Parses `query` and `top_k`.
  2. Runs the BM25 index over the registry (read-only `Arc` view).
  3. Returns a JSON object:
     ```json
     {
       "results": [
         {"name": "get_weather",
          "description": "…",
          "parameters": {…JSON Schema…}}
       ]
     }
     ```
  - Caps at `top_k` results.
  - Marks each returned name as "discovered" for the session so the
    next round's `tools=[]` includes the full schema.
  - `untrusted_output` = `false` (the result is deterministic and
    generated server-side; nothing to fence).
  - `requires_confirmation` = `Allow`.

### New router: BM25

`crates/nagent-agents/src/tools_router.rs`.

- Built once at boot from the agent registry. Holds:
  - For each agent: tokenised `(name + description + keywords)`.
  - Document frequency map for IDF.
- Public API:
  - `fn pre_select(&self, query: &str, top_k: usize) -> Vec<&'static str>`
    returning agent names ranked by score.
  - `fn search(&self, query: &str, top_k: usize) -> Vec<ToolSearchHit>`
    used by `search_tools` itself (returns full metadata).
- Tokenisation: lowercase, split on `[a-z0-9_]+`, drop a small
  English stop-word list. No stemming in v1 (keeps the dependency
  surface at zero — same as the rest of `nagent-agents`).
- Empty query / empty registry → returns empty vec.
- Empty top-k result **does not** mean "LLM has no tools": `search_tools`
  stays in `tools=[]` regardless.

### Optional `keywords()` method on `Agent`

Add one new method to `crates/nagent-agents/src/agents.rs`:

```rust
fn keywords(&self) -> &'static [&'static str] { &[] }
```

Default = empty. Lets future agents (or PRs) widen the BM25 recall
without changing the trait shape. Existing agents keep compiling
unchanged.

### Per-session discovered-tools store

`crates/nagent-server/src/llm/discovered_tools.rs`. Same shape and
lifetime as `PermissionStore` (in-memory, keyed on `chat_session_id`,
cleared on session mint).

API:

```rust
pub struct DiscoveredTools(Arc<Mutex<HashMap<Uuid, HashSet<String>>>>);

impl DiscoveredTools {
    pub fn add(&self, session: Uuid, tool: &str);
    pub fn snapshot(&self, session: Uuid) -> Vec<String>;
}
```

Read/write paths:

- Add `search_tools` and the router pre-selection on the first round
  of each turn.
- Add every successfully dispatched tool name after the loop runs
  (including the names `search_tools` itself just returned).
- Clear on session mint (mirror `PermissionStore`).

### Proxy + tool-loop changes

`crates/nagent-server/src/llm/proxy.rs` and
`crates/nagent-server/src/llm/tool_loop.rs`.

1. Boot: build the router once and stash it on `LlmState` next to
   the existing `LlmState.memory_source`. Build the
   `DiscoveredTools` store and add it to `AppState`.
2. New helper
   `build_tools_for_round(router, registry, discovered, chat_session_id, latest_user_msg) -> Vec<Value>`:
   - Always include `search_tools`.
   - Router pre-selection over the latest user message.
   - Union with the `discovered.snapshot(session)`.
   - De-duplicate by name; preserve order (search_tools first,
     then router hits, then discovered).
3. `run_tool_loop` accepts the router + the `DiscoveredTools`
   store. On each round's outgoing body:
   - Walk `body["messages"]` once to collect
     `messages[*].tool_calls[*].function.name` (any tool the LLM has
     invoked earlier in the conversation, including the previous
     round's pending calls now part of the history).
   - Call `build_tools_for_round` with that list folded in.
   - Replace `body["tools"]` on the outgoing request.
4. Dispatch logic in the existing per-call branch
   (`agents.get(&name)`):
   - If `name == "search_tools"` → resolve to the agent and call.
   - If `name` is any other registered agent → dispatch and record
     the name into `DiscoveredTools`. The current "unknown agent"
     branch stays as the catch-all.
5. SSE `tool_call` / `tool_result` frames already handle any name
   the model emits — no chat UI changes.

### Prompt rewrite

`crates/nagent-server/src/llm/prompt.rs` `DEFAULT_SYSTEM_PROMPT`:

- Remove the bullet inventory of every agent (no longer accurate
  once pre-selection is dynamic).
- Add a "tool discovery" block:
  - "`search_tools` is your main entry point: call it with a short
    natural-language query (e.g. `weather forecast`, `definition of
    X`, `convert units`) to get back the schemas for the matching
    tools."
  - "Top pre-selected tools for your message are also injected on
    every round — call them by name when you know which one fits."
  - "Do not call `search_tools` for trivial queries the pre-selected
    tools already cover (date math, weather, calculator)."
- Keep the existing per-domain rules (`memory_store` directive,
  `calculate` directive, weather card nudge, etc.). Move them into
  a "common rules" block so the rewrite doesn't drop them.
- Drop the "Tool inventory transparency" rule: with a dynamic
  registry, exhaustive enumeration belongs to `search_tools`, not
  the prose.
- Drop the "indirect prompt-injection" prose for `read_document` →
  `web_fetch`; that rule still fires (it lives in the agent's
  `requires_confirmation`), the LLM just doesn't need it in prose
  anymore. Update the
  `default_prompt_documents_user_memories_block` /
  `default_prompt_documents_user_location_block` tests to remove
  the cross-references that depended on the old inventory.
- Add new test `default_prompt_describes_search_tools` that asserts
  the new prose is present (and that the bullet inventory is gone).

Token-budget target: drop from ~6 200 to ~3 500 non-whitespace
chars. The 8 000-char guard stays in place (regression net for any
future rewrite).

### Tests

New unit tests, no DB required:

- `tools_router::tokenise` / `score` / `pre_select` / `search`
  against a 3-agent fixture (asserts the right tool wins for known
  queries and that ties are broken deterministically).
- `tool_search_agent::invoke` returns the expected schema for a
  canned query, and `top_k` is respected.
- `build_tools_for_round` composes the three sources in the right
  order, de-duplicates, and never drops `search_tools` even when the
  router returns nothing.
- `DiscoveredTools` is session-scoped (write to A, read to B returns
  empty).

Update existing tests:

- `default_prompt_lists_every_registered_agent` — delete. With dynamic
  exposure, prose enumeration is the wrong contract.
- `default_prompt_requires_exhaustive_tool_inventory_on_inquiry` —
  delete. The contract is now "LLM calls `search_tools`".
- `default_prompt_stays_under_char_budget` — keep, lower the budget
  to 5 000 to reflect the rewrite.
- `default_prompt_documents_user_memories_block` — keep, prune the
  bullet-inventory cross-checks (those bullets no longer exist).

Integration test:

- `crates/nagent-server/tests/`: new test
  `tool_search_round_trip`:
  1. POST `/v1/chat/completions` with a `stream:true` request whose
     user message is `"What is the weather in Lyon?"`.
  2. Capture the upstream `tools=[]` (the proxy injects it; assert
     `search_tools` is present and `get_weather` is in the
     pre-selected set).
  3. Feed a synthetic round that emits
     `tool_calls[0] = {name:"search_tools", arguments:"{\"query\":\"weather\"}"}`
     and asserts the tool-loop dispatches it server-side.
  4. Feed a follow-up round that calls `get_weather` by name;
     assert it dispatches without the model having to call
     `search_tools` again.

### Documentation sync (AGENTS.md §5)

Surfaces to touch:

- `docs/architecture.md` §3.1 ("Exposure"): rewrite the
  "project every agent to a `tools[]`" paragraph to describe the
  dynamic `build_tools_for_round` helper and the `search_tools`
  surface. §3.2 ("Tool loop"): add the discovered-tools store and
  the router pre-selection step to the loop description.
- `crates/nagent-agents/src/agents.rs` doc comment on the `Agent`
  trait: one line noting that `search_tools` is the user-facing
  entry point and that all other agents are reached through it.
- `README.md`: if it mentions the tool inventory in the
  configuration section, replace with a one-liner pointing at
  `search_tools`.

### Backward compatibility

- `GET /v1/agents` and `POST /v1/agents/:name/invoke` stay
  unchanged — operators and tests still hit agents by name.
- `AgentRegistry::tools_schema()` stays as a static helper for
  debugging and `/v1/agents` summary; the dynamic round-level
  helper sits next to it.
- The agent registry still lists every agent; only what reaches the
  LLM in `tools=[]` changes.

## Affected files

- `crates/nagent-agents/src/agents.rs` — new `keywords()` default
  method.
- `crates/nagent-agents/src/agents/tool_search.rs` — new agent.
- `crates/nagent-agents/src/tools_router.rs` — new BM25 router.
- `crates/nagent-agents/src/lib.rs` — re-export.
- `crates/nagent-agents/src/agents/mod.rs` (sub-module) — register
  `tool_search` in `AGENT_DESCRIPTORS` with feature
  `tool-search-agent`.
- `crates/nagent-agents/Cargo.toml` — feature flag.
- `crates/nagent-server/src/llm/discovered_tools.rs` — new store.
- `crates/nagent-server/src/llm/mod.rs` — wire it.
- `crates/nagent-server/src/llm/proxy.rs` — build router + store,
  replace static `tools=[]` injection with
  `build_tools_for_round`.
- `crates/nagent-server/src/llm/tool_loop.rs` — accept the new
  arguments, dispatch any registered name, write to
  `DiscoveredTools`.
- `crates/nagent-server/src/llm/prompt.rs` — rewrite
  `DEFAULT_SYSTEM_PROMPT`, drop inventory tests.
- `crates/nagent-server/src/state.rs` / `app.rs` — boot the router
  + the discovered-tools store, thread them into the LLM state.
- `crates/nagent-server/tests/` — new integration test.
- `docs/architecture.md` — §3.1, §3.2.
- `README.md` — only if it lists tools in the configuration
  section.

## Validation

- `cargo fmt`, `cargo build --all-targets`, `cargo clippy`,
  `cargo audit` (AGENTS.md §2).
- `cargo test --all` — new + updated unit tests pass; existing
  prompt tests pass with the rewrite.
- Manual smoke test against Ollama + Qwen2.5: ask for weather in
  Paris, observe the round-1 first round's `tools=[]` contains
  `search_tools` + `get_weather` (pre-selected); the model emits a
  `get_weather` call directly without searching. Ask for an
  ambiguous task that the router likely misses; observe the model
  call `search_tools` and then the right tool on round 2.
- `make run` + GPU stack via the `make run-cuda` / `make run-hipblas`
  paths to confirm the new dependency surface (binary does) does not
  break the GPU targets.

## Out of scope

- V2 candidates explicitly deferred:
  - ONNX embedding router (`ort` + MiniLM). Add if BM25 recall is
    insufficient in production.
  - Stemming / lemmatisation in the tokeniser.
  - Cross-conversation discovered-tools memory.
  - LLM-driven intent classification (a small classification model
    picks pre-selection before the main LLM runs).