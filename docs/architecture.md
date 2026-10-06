# nagent — Architecture

This document describes the architectural breakdown of the `nagent`
workspace. It is **descriptive, not a tutorial**: it maps the
codebase into the three primary subsystems (API, agents, tools)
and explains how they interact at runtime, with concrete file
references.

It is the canonical reference for "where does this code live?" and
"what is this crate responsible for?". New contributors must read
it before working on the codebase (see `AGENTS.md` §"Required
reading"); reviewers must update it whenever the breakdown changes
(see `AGENTS.md` §5 — Documentation Sync).

## Workspace layout

The workspace has six crates, listed under `Cargo.toml`
`[workspace.members]` in dependency order:

| Crate | Role | Layer |
| --- | --- | --- |
| `crates/stt-proto` | Wire types and `postcard` codec for the WS protocol. | leaf |
| `crates/stt-core` | `WhisperBackend` trait, `whisper-rs` implementation, inference worker and queue. | infra |
| `crates/nagent-support` | Shared small types and utilities used by `nagent-server` (TTL-map, bounded-blocking, sweep-clock primitives). | infra |
| `crates/nagent-db` | `Db` wrapper around `sqlx::Pool` plus migrations / row types shared by auth, credentials, and documents. | infra |
| `crates/nagent-agents` | Chat-agent subsystem (§2). Pure: no dependency on `nagent-server`, `sqlx`, or `whisper-rs`. | domain |
| `crates/nagent-server` | The `axum` HTTP/WebSocket server (§1, §3). The only binary in the workspace. | transport |

Dependency direction is strictly top-down: `nagent-server` is the
only crate that pulls in `nagent-agents` and `nagent-db`. Agents
reach the credentials and documents subsystems exclusively through the
`SecretSource` and `DocumentSource` capability traits so the crate
boundary stays clean (see §2.2).

## 1. API (HTTP / WebSocket transport)

**Files:** `crates/nagent-server/src/http/`,
`crates/nagent-server/src/app.rs`,
`crates/nagent-server/src/main.rs`,
`crates/nagent-server/src/stt/`.

The API layer is the `axum::Router` assembled by
[`http::build_router`](../../crates/nagent-server/src/http/mod.rs)
from a set of `mount_*` helpers, one per subsystem. The router is
the composition root for every external surface; nothing else in
the codebase reaches the network.

### 1.1 Composition

`http::build_router` reads top-to-bottom:

1. **Public subtree** — `/`, `/healthz`, `/api/version`, `/static/*`.
   No auth required. Operators can scrape the health probe and
   version probe without a session, and the browser can load the
   login page before authenticating.
2. **Protected subtree** — every other route, gated by `RequireAuth`
   when `auth.enabled = true`. Each per-subsystem `mount_*` helper
   returns a `Router<Arc<AppState>>` already wrapped in the shared
   `/v1/*` envelope (CORS, per-IP rate limit, bearer-auth gate).
   The subtree is built by conditionally merging:
   - `mount_features` — `GET /api/features` (feature discovery).
   - `mount_agents` — `GET /v1/agents`, `POST /v1/agents/:name/invoke`.
   - `mount_llm_proxy` — `POST /v1/chat/completions`, `GET /v1/models`.
   - `mount_documents` — Discussion-mode document upload/download.
   - `mount_chat_sessions` — `POST /v1/chat/session` (chat-session id mint).
   - `mount_tts` — `POST /v1/audio/speech`, `GET /v1/audio/voices`.
   - `GET /ws` — the STT WebSocket upgrade.
3. **Auth subtree** (when `auth.enabled = true`) — login routes
   (`/api/auth/login/*`) merge into `public`; identity routes
   (`/api/me`, `/api/auth/logout`, passkey register) and credentials
   routes (`/v1/credentials/*`) merge into `protected` and are
   wrapped with `RequireAuth`.
4. **Outermost layers** — security headers
   (`security_headers::*`) plus the access log. Applied as the
   outermost layer so a single header copy runs regardless of
   which subtree handled the request.

### 1.2 State

[`AppState`](../../crates/nagent-server/src/state.rs) is a single
`Arc`-shared struct built once per process by
[`app::build_app`](../../crates/nagent-server/src/app.rs). It is
composed of sub-state groups (`SttState`, `LlmState`, `AuthState`,
`DocumentsState`, `ChatSessionsState`, `TtsState`, agents,
services). Every axum handler extracts its sub-state through
`FromRef<Arc<AppState>>`, so the whole router keeps the
`Router<Arc<AppState>>` type without per-handler state plumbing.

`AppState` is `Clone`-cheap: every sub-state group is internally an
`Arc<...>`, and `Arc::clone` is the only work the clone does. That
is what makes it safe to share one state instance across every
concurrent axum task.

### 1.3 Subsystems that plug into the API

The API layer is plumbing — it does not implement business logic.
The subsystems it stitches together:

| Subsystem | Module | Role |
| --- | --- | --- |
| STT | `stt/` | WebSocket upgrade, session map, whisper backend, watchdog. |
| LLM proxy | `llm/` | Optional OpenAI-compatible proxy to a local LLM. Hosts the tool loop (§3). |
| Agents | `agents/` + `nagent-agents` | Chat-agent subsystem (§2). |
| Documents | `documents/` | Discussion-mode document uploads, extraction, the `read_document` tool. |
| Auth | `auth/`, `credentials/`, `oauth/`, `memories/` | Multi-user auth (password / OIDC / passkey) + per-user credentials vault + per-user UI preferences row (location / timezone sharing opt-ins + reply-language picker, migration `0007`; additional-instructions textarea + per-turn temperature via migration `0008`; long-term memory opt-in via migration `0009` + `memories` table via `0010`). |
| TTS | `tts/` | Local Piper text-to-speech engine. |
| Static frontend | `static/`, `static_assets.rs` | Vendored UI served by the static handler. |

## 2. Agents (chat-agent subsystem)

**Crate:** [`crates/nagent-agents`](../../crates/nagent-agents) — no
dependency on `nagent-server`, `sqlx`, or `whisper-rs`.

The agents crate is the entire agent subsystem: the trait, the
error type, the `UserContext`, the `AgentRegistry`, the per-agent
`*Config` structs, the service catalogue, the egress client, and
every agent implementation. The server crate wraps the static factory
table in `AgentRegistryFactory` so it can attach the
`read_document` agent whose `DocumentSource` impl needs the
server-side `DocumentStore`.

### 2.1 The `Agent` trait

File: [`nagent-agents/src/agents.rs`](../../crates/nagent-agents/src/agents.rs).

```rust
#[async_trait]
pub trait Agent: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    async fn invoke(&self, ctx: &UserContext, args: Value)
        -> Result<String, AgentError>;
    fn requires_confirmation(&self, ctx: &UserContext, args: &Value)
        -> ConfirmationDecision;
    fn untrusted_output(&self) -> bool;
}
```

Each agent is a single `Arc<dyn Agent>`. The trait methods are the
full contract between an agent and the rest of the system:

- `name` / `description` / `parameters_schema` are projected to the
  LLM as the `tools[].function` block (see §3.1).
- `invoke` returns a JSON-encoded string fed back to the LLM as
  `role: "tool" content`.
- `requires_confirmation` enforces per-call policy without the
  tool loop having to know about any agent by name (see §3.3).
- `untrusted_output` decides whether the LLM tool loop wraps the
  result in an untrusted-input fence (see §3.5).

### 2.2 `UserContext`

[`UserContext`](../../crates/nagent-agents/src/agents.rs) is
constructed once per LLM tool round (or once per direct
`/v1/agents/:name/invoke` call). It owns the `SecretCache`, the
`SecretSource` (credential resolver) `Arc`, the `chat_session_id`
propagated from the browser, and the `invoked_this_turn` vector
consumed by `requires_confirmation`. Its `Drop` impl zeroises the
plaintext cache so no credential outlives the request.

The crate boundary is enforced by three capability traits:

- `SecretSource` — `async fn fetch(user_id, service, field)`. The
  only thing the agents crate sees of the credentials subsystem
  on the **read** path. `nagent-server`'s `ResolverSecretSource`
  adapts the `CredentialResolver` to it.
- `SecretSink` — `async fn update(user_id, service, fields)` (plan
  1790695073418). The write-side mirror; the agents crate can
  refresh a per-user secret back into the vault through this
  trait without ever seeing the encryption key. `nagent-server`'s
  `SecretSinkImpl` (next to the resolver) writes one audit row
  of kind `credential_access` on every successful update so the
  audit log reads uniformly across reads and writes.
  `UserContext::sink` is `None` outside the chat-session
  constructor so test / direct-invoke paths stay sink-free.
- `DocumentSource` — `async fn read(user_id, chat_session_id, name)`.
  The only thing the agents crate sees of the documents
  subsystem. `nagent-server`'s `StoreDocumentSource` adapts the
  `DocumentStore`.

### 2.3 `AgentRegistry`

[`AgentRegistry`](../../crates/nagent-agents/src/agents.rs) is a
`Clone`-cheap `Arc<Vec<Arc<dyn Agent>>>` built by
`AgentRegistry::from_config(&AgentConfigs, enabled)`, where
`enabled` is the operator master switch (`AGENTS_ENABLED=true`).
Each per-agent cargo feature gates the corresponding `push_agent`
call so a slim build drops the heavy machinery — "one feature per
agent".

### 2.4 Built-in agents

File: `crates/nagent-agents/src/agents/*.rs`.

| Agent | Cargo feature | Description |
| --- | --- | --- |
| `web_fetch` | `web-agent` | Server-side HTTP GET → clean text. Hardened egress client (SSRF policy, DNS-rebinding mitigation, allow-list). |
| `read_document` | `read-document-agent` | Fetches a single uploaded document by name, scoped to the current chat session. |
| `get_datetime` | `datetime-agent` | Pure local date math (`chrono`, `chrono-tz`). |
| `calculate` | `calculate-agent` | Pure local expression evaluator (`meval`). |
| `unit_convert` | `unit-convert-agent` | Pure local conversion tables. |
| `get_weather` | `weather-agent` | WeatherAPI.com (free API key required). |
| `wikipedia` | `wikipedia-agent` | `wikipedia.org` REST, no API key, `User-Agent` set. |
| `dictionary` | `dictionary-agent` | Free Dictionary REST API, no API key. |
| `get_stock_quote` | `stock-agent` | Stooq CSV, no API key. |
| `caldav_list_events` | `caldav-agent` | Per-user CalDAV calendar: list `VEVENT`s in a time range. Confirm-on-read — calendar contents are sensitive. |
| `caldav_get_event` | `caldav-agent` | Per-user CalDAV calendar: fetch a single `VEVENT` by `UID`. Confirm-on-read. |
| `caldav_create_event` | `caldav-agent` | Per-user CalDAV calendar: append a new `VEVENT` (confirm-on-write). |
| `x_timeline` | `x-agent` | Per-user X (Twitter) home timeline via the v2 API (mode "Abonnements" / "Pour Vous"). Read-only. Refreshes the OAuth access token itself on 401. |
| `memory_store` | `memory-agent` | Persist one durable fact for the calling user (AES-256-GCM encrypted at rest, dedup on `(subject, predicate)`). Idempotent — re-storing the same fact returns the same memory id. |
| `memory_recall` | `memory-agent` | Decrypt + return up to `recalled_top_k` rows matching the supplied `subject` / `predicate` / `tags` LIKE filters. |
| `memory_list` | `memory-agent` | List metadata-only rows for the calling user. Pairs with `memory_recall` to retrieve the value of a specific id (the list shape never includes plaintext). |
| `memory_forget` | `memory-agent` | Forget one memory by id. `NeedsConfirmation` (even though reversible through `memory_store`) so the chat UI surfaces a "forget this memory?" bubble. |
| `config_doc` | `web-agent` (same as `web_fetch`) | Returns the LLM-facing description of the per-user service catalogue. |

Each agent declares its `untrusted_output` impl at the type level:
pure-local agents return `false`, every other agent returns `true`.

The CalDAV plugin (plan 1790963194218) ships **read + add only** in
v1 — `caldav_update_event` and `caldav_delete_event` are explicitly
forbidden (the `CalDavClient` does not expose `update()` / `delete()`
methods and no such agent is registered in `AGENT_DESCRIPTORS`).
`caldav_list_calendars` is a setup-only HTTP endpoint
(`POST /api/integrations/caldav/probe-calendars`), not an LLM tool.
See `docs/integrations/caldav.md` for the operator-facing guide.

### 2.5 Direct HTTP routes

[`nagent-server/src/agents/routes.rs`](../../crates/nagent-server/src/agents/routes.rs)
exposes two direct endpoints that bypass the LLM tool loop:

- `GET /v1/agents` — list every registered agent
  (`{"data": [...]}`). Used by the browser to render the
  "Integrations" tab.
- `POST /v1/agents/:name/invoke` — direct invocation. Body
  `{"arguments": {...}}`; response
  `{"name": "<agent>", "result": "<json string>"}`. Used by
  curl, integration tests, and any non-streaming consumer.

The chat UI does not use these routes — it goes through
`/v1/chat/completions` so the SSE stream stays consistent.

### 2.6 Services are not agents

`ServiceRegistry` (`nagent-agents/src/services.rs`) is the catalogue
of *integrations* the user has configured (e.g. "WeatherAPI key").
Agents consume services through `UserContext::secret(service,
field)`. The catalogue is plain data — no I/O — so it lives in
`nagent-agents` for the same reason the rest of the agents
subsystem does.

When the `caldav-agent` cargo feature is on, the registry
contains the `caldav` `ServiceDef` (id `"caldav"`, fields `url`,
`username`, `password`). The runtime probe endpoint
(`POST /api/integrations/caldav/probe-calendars`) lets the
integrations UI discover the user's calendar collection
before saving it; the chat agents read the saved `url` as
the calendar collection URL.

When the `x-agent` cargo feature is on, the registry also
contains the `x_account` `ServiceDef` (id `"x_account"`, six
fields: `access_token`, `refresh_token`, `token_scope`,
`x_user_id`, `x_screen_name`, `token_expires_at`). The OAuth
flow at `/api/auth/login/x/{start,callback,disconnect}`
populates the row at connect time; the `x_timeline` agent
refreshes the token-shaped fields in place on 401 through
`UserContext::update_secret(...)`. See
`docs/integrations/x.md` for the operator-facing guide.

#### 2.7 Setup-only helpers

The CalDAV connector is the first feature that ships a
*setup-only* HTTP helper — an endpoint reachable from the
integrations UI but not from the LLM tool loop. The pattern:

- A `ServiceDef` describes the per-user fields the user
  must supply (`url`, `username`, `password`).
- A `POST` endpoint accepts credentials **in the request
  body**, validates the principal host against the same
  allowlist the chat agents use, and returns a list of
  selectable resources (here: PROPFIND results filtered to
  `<C:calendar/>`).
- The UI lets the user pick one, then PUTs the chosen
  resource's href as the saved `url` field through the
  existing `PUT /api/integrations/:id/credentials` flow.
- A `caldav_probe_<outcome>` audit row records the probe
  attempt (host + outcome); the password never lands on
  the audit row.

Setup-only helpers are intentionally **not** registered in
`AGENT_DESCRIPTORS` — the LLM tool loop returns
`"unknown tool"` if a model tries to call them. The
hardening posture is the same as the chat agents
(operator-set allowlist, fail-closed default), but the
endpoint runs on the **server** with the supplied
credentials (not from the vault — the user is choosing
what to put in the vault).

## 3. Tools (LLM function-calling integration)

**Files:** `crates/nagent-server/src/llm/`,
[`crates/nagent-agents/src/agents.rs`](../../crates/nagent-agents/src/agents.rs)
(the trait side).

The "tools" subsystem is the bridge between the agent
implementations (§2) and the upstream LLM: how agents are exposed
as OpenAI-style callable tools, how the LLM's tool-call deltas are
parsed, and how the resulting dispatch is woven into the SSE
response stream.

### 3.1 Exposure

Plan 1791317253718: instead of projecting every agent into a
flat `tools=[]` on every round, the LLM sees a small dynamic
surface composed of three sources via
`llm::proxy::build_tools_for_round`:

```
[search_tools] + router_pre_select(top_k_naming) + already_discovered(session)
```

1. `search_tools` is the user-facing tool-discovery meta-agent
   (the Anthropic-style "tool search" pattern). It is always
   present.
2. The BM25 pre-selection over the latest user message is
   produced by `nagent_agents::tools_router::ToolsRouter` — a
   small hand-rolled BM25 (no stemming, no embeddings; matches
   the crate's "no external services" posture) that scores
   `name + description + keywords` against the query. Default
   `top_k` = 5.
3. The per-session
   `llm::discovered_tools::DiscoveredTools` store tracks the
   tool names the LLM has already invoked in the current
   `chat_session_id` so the round-level rebuild keeps the
   discovered JSON Schemas live (most upstreams forget tools
   that disappear from `tools=[]` between rounds).

`AgentRegistry::tools_schema()` is still available as a static
helper for `/v1/agents` and debugging; the round-level helper
sits next to it and is what `chat_completions` actually calls.

The proxy (`llm/proxy.rs::chat_completions`) merges the dynamic
array with the client-supplied `tools`, server tools take
precedence on collision, and injects the result into the
upstream request body on every round. The tool loop
re-applies the same builder before each round-after-the-first so
router hits + discovered tools stay authoritative.

### 3.2 The tool loop

[`llm::tool_loop::run_tool_loop`](../../crates/nagent-server/src/llm/tool_loop.rs)
drives the per-chat-completion round trip:

1. Open (or reuse) an upstream `/v1/chat/completions` connection.
2. Drain the SSE byte stream (`llm::sse::drain_upstream_round`),
   buffering every `tool_calls` delta and forwarding everything
   else verbatim to the client channel.
3. If the LLM emitted one or more tool calls, dispatch each one
   through `AgentRegistry`, append a `role: "tool"` message with
   the result, and loop. On every successful dispatch, record
   the agent name into the per-session
   `llm::discovered_tools::DiscoveredTools` store so the next
   round's `tools=[]` keeps the schema live.
4. Before each round-after-the-first that opens a fresh upstream
   connection, rebuild the outgoing body's `tools=[]` via
   `build_tools_for_round` with the live router, the per-session
   discovered set, and the `tool_calls` history (the
   round-after-discovery gap fallback).
5. When the LLM emits `finish_reason: "stop"` (no tool calls),
   close the channel with `data: [DONE]\n\n`.

Round 1 reuses the body stream the proxy already opened; round
1+ open their own connection through the shared `reqwest` client.
The function returns when the conversation is complete or when an
unrecoverable error occurs; either way `tx` is dropped before
return so the axum body stream terminates cleanly.

### 3.3 Dispatch and the indirect prompt-injection rule

For every tool call the LLM emits:

1. Look up the agent by name in `AgentRegistry`. Unknown names
   produce a synthetic tool-result error fed to the LLM so the
   model can recover, not a 500 to the browser.
2. Build a `UserContext` for the authenticated user and current chat
  session. The current implementation creates a new context for
  each policy check and invocation; credential injection and
  per-turn history are not yet wired (see §4, Phases 0 and 2).
3. If `Agent::requires_confirmation(ctx, args)` returns
   `NeedsConfirmation { reason }`, emit `role: "tool"` with the
   reason verbatim — the LLM is expected to ask the user in plain
   text, and the next round will see `Allow`.
4. Otherwise call `Agent::invoke(ctx, args)`, JSON-encode the
   result, and emit it.
5. If `Agent::untrusted_output()` is `true`, wrap the
   `role: "tool"` content in an untrusted-input fence before
   sending the next round's request body to the upstream.

The cross-agent confirmation policy (e.g. "`web_fetch` after
`read_document` requires user confirmation") lives in the agent's
own `requires_confirmation` impl — the tool loop does not need to
know agent names. The current loop does not preserve invocation
history between calls, so this policy is not reliably enforced yet
(see §4, Phase 0).

### 3.4 SSE framing

The loop pushes output into the client channel as either verbatim
upstream SSE bytes or locally synthesised `event: tool_call` /
`event: tool_result` frames. The forwarder in `llm::sse` keeps
the upstream `[DONE]` sentinel private (it swallows any `[DONE]`
it sees from upstream and emits its own sentinel when the
conversation actually completes) so the browser cannot cut the
response mid-loop and drop subsequent `event: tool_call` /
`event: tool_result` frames. The regression is documented inline
in the file.

### 3.4.1 Permission flow (chat-only)

When an agent's `requires_confirmation` returns
`NeedsConfirmation`, the tool loop now pushes a `PendingApproval`
entry into a per-session [`PermissionStore`](../../crates/nagent-server/src/llm/permission.rs)
keyed on the tool call id, and emits an enriched `tool_result`
SSE frame carrying `needs_approval: true` plus a pre-formatted
`prompt` object (`title`, `body`, `danger` ∈ {`low`,`medium`,
`high`}) so the chat UI can render an inline approval card with
three buttons.

- **Direct-invoke route** (`POST /v1/agents/:name/invoke`) is
  **unchanged**: direct calls bypass the chat SSE flow entirely
  and have no UI affordance. The permission flow is a chat-only
  concern.
- **`Agent::requires_confirmation`** trait contract is preserved;
  the bypass happens at the tool-loop call site by checking the
  session-wide override set before consulting the trait method.
  This keeps the direct-invoke route honest and respects the
  existing trait contract.
- **Chat route intercept**: `llm::proxy::chat_completions` runs
  the user's latest message through
  `parse_decision_prefix` BEFORE the upstream round begins. On a
  recognised sentinel (`[APPROVE:tool_call_id]`,
  `[APPROVE_ALWAYS:tool_name]`, `[DENY:tool_call_id]`), the
  route drives the matching action:
  - `Approve`: pop the pending entry, invoke the agent directly
    with `force_allow` semantics (no `requires_confirmation`
    check), emit synthetic `tool_call` / `tool_result` SSE
    frames, and append the matching `tool_calls[]` +
    `role: "tool"` entry to the body so the LLM summarises.
  - `ApproveAlways`: insert `tool_name` into the session's
    override set; if a pending entry exists for that name, drive
    it as `Approve`; otherwise let the LLM ack on the next round.
  - `Deny`: drop the pending entry and emit a synthetic
    `tool_result { ok=false, content="Utilisateur refusé" }`; the
    LLM picks the denial up as a normal tool error and writes a
    one-line acknowledgment.
- **Stale or unknown id** (`[APPROVE:abc]` where `abc` was never
  registered): the chat route emits a synthetic tool error
  (`[error] approval expired or unknown; please ask the user
  again`) so the LLM can re-prompt.
- **Persistence**: `PermissionStore` is in-memory only and is
  cleared on session mint. A server restart drops both pending
  entries and overrides, matching the LLM-mediated path's own
  lack of state. A future plan can persist the override set to
  `nagent_db` if needed.

The detailed wire-level diff lives in plan
`.kilo/plans/1791229183545-tool-bubble-footer-pill.md` §2.6.

### 3.5 Hardening

- **Privacy** — `llm::privacy` is the boundary that strips
  per-user PII from outbound log lines and from any request body
  forwarded to the upstream. The rule is the same for every
  upstream call: tool calls don't add privacy risk, they just
  get the same treatment.
- **Rate limit** — the shared `/v1/*` envelope
  (`http::llm_guards::llm_rate_limit_middleware`) caps per-IP LLM
  traffic. The auth subtree has its own dedicated limiter at
  `auth::login_rate_limit`.
- **Schema validation** — the agent's `parameters_schema()` is the
  only schema the upstream ever gets. There is no client-supplied
  schema for server tools, and the tool loop never builds
  request bodies on the client's behalf.

## Cross-cutting

- **Configuration** — every runtime setting is either a Cargo
  feature, an env var, a Dockerfile build arg, or a TOML file
  passed via `--config <path>`. See `README.md` §"Configuration"
  for the precedence table (`env > TOML > default`).
- **Tests** — agent unit tests live next to each agent
  implementation; integration tests live under
  `crates/nagent-server/tests/`; the boot path is exercised
  through `crates/nagent-server/src/testing.rs`
  (feature-gated behind `test-util`).
- **Boot** — `app::build_app` is the single composition root that
  builds `Arc<AppState>` from a `&Config`. Tests can swap pieces
  in/out through `testing` instead of re-implementing the chain.