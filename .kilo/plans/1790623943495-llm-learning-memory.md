# Plan — LLM self-learning memory (user facts + world/tool facts)

Make the LLM learn from what it observes across turns on two planes, in a
lightweight way that fits the existing codebase:

1. **User plane** — facts the LLM learns about the user (name, preferences,
   timezone, language, recurring topics) so it can serve them better next turn.
2. **Knowledge plane** — reusable facts and tool-usage hints the LLM picks up
   from tool results (web pages, encyclopedia entries, calculator quirks,
   weather API shape, etc.) so it improves its own future tool calls.

The server is **per-turn stateless** (`chat.js` re-sends `messages[]`, see
`crates/stt-server/src/static/chat.js` and the tool loop at
`crates/stt-server/src/llm.rs:442`). Memory is therefore injected at request
time and written asynchronously after dispatch.

---

## Design summary

- **Two explicit tools** the LLM can call itself:
  - `remember_fact(kind, content, subject?, source?)` — write a fact.
  - `recall(query, kind?, limit?)` — search the memory store.
  - `forget_fact(id)` — soft-delete (user-facing control).
- **Opt-in background observer** that, after each tool dispatch, asks a cheap
  classifier prompt whether the tool result is worth remembering as a
  `world_fact` or `tool_hint`. **Off by default** (`memory.observer_enabled`).
- **At request time**, top-K relevant memories for the user are injected into
  the system prompt in a `<memory>` block, right after the admin prompt.
- **Storage**: reuse the existing `AuthStore`-style DB abstraction
  (`crates/stt-server/src/auth/store.rs` + `db_sqlite.rs` + `db_postgres.rs`),
  with a new migration `0002_memory.sql`. No embeddings, no vector DB, no
  new dependency — plain text `LIKE` retrieval in v1.

`auth.db` → `nagent.db` rename is out of scope here; flagged as a follow-up.

---

## Constraints and decisions

- **Identity & RBAC**: memory requires auth (`users.id`); writes and reads
  are always scoped to the calling user. Every storage query MUST filter on
  `user_id = ?` so users can never see or mutate each other's rows. The
  multi-user plan's **PR2** (AgentContext + scopes + audit sink) is a hard
  prerequisite for the scope names below; this plan lands once PR2 is in.
  Scope contract that the memory agents must declare at registration:
  - `remember_fact` requires `"agent:memory:write"`.
  - `recall` requires `"agent:memory:read"`.
  - `forget_fact` requires `"agent:memory:write"` (user-side deletion).
  When `AGENTS_ENABLED=false` *and* auth is off, the memory tools return a
  clear "memory disabled" error. Anon-cookie identity is a follow-up.
- **Cargo feature**: `memory-agent`, off by default. Opted in by the same
  `make run-llm` convention used for the LLM feature today.
- **Supersedes the F1 memory sketch in the multi-user plan**: this plan is
  the canonical design for the `memory-agent` feature. The multi-user plan's
  F1 still owns the sibling identity/audit agents (`whoami`, `audit_log_self`)
  and the RBAC contract (scopes, audit sink); F1's per-user isolation
  guarantee (`WHERE user_id = ?`) is restated in the **Identity & RBAC**
  bullet above and is non-negotiable here. The `memories` table replaces the
  F1 `user_memory(user_id, key, value, embedding BLOB, updated_at)` sketch
  — same per-user isolation, plus `confidence`, `use_count`, `last_used_at`,
  `superseded_by` for the LLM-learning use case. Embeddings remain deferred
  (out of scope below).
- **Capture** is always opt-in: explicit via the tools, plus the observer
  only when the operator enables it. Raw user messages are never auto-facts.
- **Bounded**: `memory.max_user_facts` (default 50, FIFO prune) and
  `memory.max_world_facts` (default 200) cap the corpus per user. The
  injected block is truncated to ~1500 tokens.
- **Privacy**: no content leaves the server, no third-party embedding call,
  `forget_fact` is a tool the user can ask the LLM to invoke.

---

## Data model — `0002_memory.sql`

```sql
CREATE TABLE memories (
    id            TEXT PRIMARY KEY,           -- uuid v4
    user_id       TEXT NOT NULL,              -- FK -> users.id (auth)
    kind          TEXT NOT NULL,              -- 'user_fact' | 'world_fact' | 'tool_hint'
    subject       TEXT,                       -- e.g. user name, domain, tool name
    content       TEXT NOT NULL,
    source        TEXT,                       -- tool name | 'user' | 'classifier'
    source_url    TEXT,                       -- optional, e.g. fetched URL
    confidence    REAL NOT NULL DEFAULT 1.0,
    use_count     INTEGER NOT NULL DEFAULT 0,
    last_used_at  TEXT,
    created_at    TEXT NOT NULL,              -- ISO 8601
    superseded_by TEXT REFERENCES memories(id)
);
CREATE INDEX memories_user_kind_idx ON memories(user_id, kind);
CREATE INDEX memories_subject_idx   ON memories(user_id, subject);
```

v1 retrieval uses `WHERE user_id = ? AND (subject LIKE ? OR content LIKE ?)
ORDER BY last_used_at DESC NULLS LAST LIMIT ?`. FTS5 / `tsvector` is a
follow-up once the corpus proves worth indexing.

---

## Code changes (ordered)

1. **Cargo feature**
   - `crates/stt-server/Cargo.toml`: add `memory-agent = []` (no new deps).
   - In `crates/stt-server/src/lib.rs`, `pub mod memory;` guarded by
     `#[cfg(feature = "memory-agent")]`.

2. **Config** — `crates/stt-server/src/config.rs`
   - New `MemoryConfig { enabled, observer_enabled, max_user_facts,
     max_world_facts, classifier_max_per_min, injection_token_budget }`.
   - Defaults: all off, 50/200, 30/min, 1500 tokens.
   - Env vars: `MEMORY_ENABLED`, `MEMORY_OBSERVER_ENABLED`,
     `MEMORY_MAX_USER_FACTS`, `MEMORY_MAX_WORLD_FACTS`.
   - `AppState` gets `pub memory: Option<MemoryHandle>`.

3. **TOML overlay** — `crates/stt-server/src/config_file.rs`
   - Add `TomlMemoryConfig` and wire it under `[memory]` in
     `docs/examples/config.toml.example` (and update the file's docstring).

4. **Store** — `crates/stt-server/src/memory/mod.rs`
   - `trait MemoryStore { fn upsert(...), fn search(...), fn forget(...),
     fn prune_for_user(...) }`.
   - `db_sqlite.rs` and `db_postgres.rs` mirror the auth pattern
     (`crates/stt-server/src/auth/db_sqlite.rs`).
   - `MemoryHandle` wraps the store + a `tokio::sync::mpsc::Sender<Observe>`
     for the observer worker.

5. **Schema migration**
   - New `crates/stt-server/migrations/0002_memory.sql`.
   - Apply on startup in `main.rs`, right after the existing migration step
     (mirror the auth migration bootstrap).

6. **Tools** — `crates/stt-server/src/memory/agents.rs`
   - `RememberAgent`, `RecallAgent`, `ForgetAgent` implement the existing
     `Agent` trait from `crates/stt-server/src/agents/mod.rs:74`.
   - Schema names/descriptions live next to the trait so the tool list in
     `docs/ui_features.md` and the system prompt stay in sync.

7. **Registry** — `crates/stt-server/src/agents/mod.rs`
   - In `AgentRegistry::from_config`, when `cfg!(feature = "memory-agent")`
     and `memory.enabled`, register the three memory agents.
   - `AgentRegistry::tools_schema()` already merges them automatically
     (used at `llm.rs:224`).

8. **System prompt injection** — `crates/stt-server/src/llm_prompt.rs`
   - New helper `inject_memory_context(messages, store, user_id, budget)`
     that prepends a `<memory>` system block after `inject_default_system_prompt`.
   - Block format:
     ```
     <memory user_id=...>
     user_facts:
     - [id=…, subject=…] content   (use_count=N, last_used=…)
     world_facts / tool_hints (top-K by query match):
     - …
     </memory>
     ```
   - Update the unit tests in `llm_prompt.rs:164-409` so the snapshot
     reflects the new block (and the absence of the block when
     `memory.enabled = false`).

9. **Dispatch capture** — `crates/stt-server/src/llm.rs`
   - After `agent.invoke(args).await` at `llm.rs:619`, build an `Observe`
     (`tool_name`, `args`, `result_summary`, optional `source_url` extracted
     from args when the tool is `web_fetch`).
   - Send through `state.memory.observe_tx` if present; drop on overflow
     (bounded channel, e.g. 256). Never block the response.

10. **Observer worker** — `crates/stt-server/src/memory/observer.rs`
    - Spawned in `main.rs` when `memory.observer_enabled`.
    - Drains `Observe`s, rate-limits classifier calls (token bucket),
      calls the LLM with a small classifier prompt:
      "Return JSON `{remember: bool, kind, content}` if this tool result is
      reusable across future turns, else `{remember: false}`."
    - Upserts via `MemoryStore` with a dedup check
      (`existing.similar_to(content)` → bump `use_count` instead of insert).

11. **Wire-up in `main.rs`**
    - Build `MemoryStore` after `AuthStore`, share the same `sqlx::Pool`.
    - Spawn observer task. Pass `Option<MemoryHandle>` into `AppState`.

12. **Docs**
    - `docs/examples/config.toml.example`: full `[memory]` section with
      comments on each knob.
    - `README.md`: short subsection under the LLM section explaining the
      two planes and how to enable.

---

## Failure modes

- **DB unreachable** → log once, skip injection, do not fail the chat.
- **Classifier fails / times out** → log, drop observation, continue.
- **`remember_fact` fails** → return `AgentError` so the LLM sees the error
  in its tool bubble (no silent loss).
- **Pruning** runs lazily on insert (`max_user_facts` / `max_world_facts`):
  delete the oldest low-`use_count` row for that user/kind.
- **Bounded channels** on the observer drop on overflow with a counter in
  metrics (log line).

---

## Validation

- `cargo fmt && cargo clippy --all-targets -- -D warnings`
- `cargo test -p stt-server` covering:
  - store CRUD, dedup, pruning, limit enforcement (sqlite + postgres if CI
    has it; otherwise sqlite-only with a `#[ignore]` pgsql test).
  - `inject_memory_context` produces the expected block and is a no-op when
    disabled / when user has no memories.
  - `remember_fact` followed by a second request injects the fact into the
    system prompt (assert against the assembled `forward_body` before the
    upstream call).
  - `forget_fact` removes the entry from the next request's injection.
- Update the existing `llm_prompt.rs` snapshot tests to include the new
  block when enabled and to remain byte-identical when disabled.
- Manual smoke: `make run-llm` with `memory.enabled=true`, ask the LLM
  "remember that I prefer Celsius" → reopen the page → ask "what unit do I
  prefer" → confirm the LLM recalls.

---

## Out of scope (follow-ups)

- `auth.db` → `nagent.db` rename (touches URLs, env vars, kustomize, README).
- Embeddings / vector retrieval (only if `LIKE` proves insufficient at
  realistic corpus sizes).
- Anon-cookie identity when auth is off.
- Cross-user shared knowledge (would need a `kind = 'shared_*'` and a
  separate read path).
- Automatic summarization of long conversations.
- Web UI for browsing/forgetting memories (tool-only for v1).
