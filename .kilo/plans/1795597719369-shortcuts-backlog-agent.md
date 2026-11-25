# Shortcut.com agent — read the user's backlog (stories, epics, iterations) so the LLM can plan from it

Status: proposal, written 2026-11-25 (step 4 of the session). No code
changes yet. Initially drafted as an Apple-Shortcuts CLI bridge; the
user clarified the target is **https://app.shortcut.com** — the
Shortcut project-management app (ex-Clubhouse) — so this plan is now
a REST API agent, same shape as the GitHub one.

## API facts (verified 2026-11-25 against developer.shortcut.com/api/rest/v3)

- Base URL: `https://api.app.shortcut.com/api/v3`
- Auth: `Shortcut-Token: <api-token>` header (token generated at
  `app.shortcut.com/settings/account/api-tokens`); OAuth 2.0 exists
  but is out of scope for v1 — API token only, like the GitHub PAT.
- Rate limit: 200 requests/minute per token.
- Resources: members, teams, workflows, projects, epics, iterations,
  milestones, labels, categories, stories (+ story search, comments,
  tasks).

## Goal

A `shortcut-agent` cargo feature that adds a per-user **Shortcut**
integration (API token in the existing credentials vault) and
**read-only** chat tools:

| Tool | Endpoint | Args |
| --- | --- | --- |
| `shortcut_me` | `GET /members/me` | — |
| `shortcut_list_workflows` | `GET /workflows` | — (state id → name/category map, needed to interpret backlog) |
| `shortcut_list_projects` | `GET /projects` | `limit?` |
| `shortcut_list_epics` | `GET /epics` | `state?` (unstarted/started/done), `limit?` |
| `shortcut_get_epic` | `GET /epics/{id}` | `epic_id` (description capped) |
| `shortcut_list_iterations` | `GET /iterations` | `limit?` (current + upcoming) |
| `shortcut_search_stories` | v3 story-search endpoint (`query`, `page_size`) | `query` (Shortcut search syntax), `limit?` |
| `shortcut_list_backlog` | story search with the configured backlog query | `limit?` (default 50, cap 200) — the core "read my backlog" tool |
| `shortcut_get_story` | `GET /stories/{id}` | `story_id` (description/comments capped) |
| `shortcut_list_labels` | `GET /labels` | `limit?` |

`shortcut_list_backlog` runs the operator-configured
`[agents.shortcut].backlog_query` (default: stories in an unstarted
workflow state and not in an iteration — pin the exact search syntax
against the v3 docs at implementation time; Shortcut search supports
`is:story`, workflow-state and iteration predicates).

Write tools (create story / comment / move story) are **out of scope
for v1** — read-only, same as the GitHub plan.

Intended workflow: "read my Shortcut backlog and propose a plan for
the next iteration" → LLM calls `shortcut_me` → `shortcut_list_workflows`
→ `shortcut_list_backlog` / `shortcut_search_stories` →
`shortcut_get_story` / `shortcut_get_epic` on the relevant items →
answers with a prioritised plan.

## Patterns to mirror (all landed 2026-11)

- `crates/nagent-agents/src/x_account_service.rs` — `ServiceDef`
  field layout.
- `crates/nagent-agents/src/agents/x_timeline/mod.rs` — secret
  access via `ctx.secret`, shared egress pool, cache, error mapping.
- `crates/nagent-server/src/credentials/email_probe.rs` +
  `crate::probe::ProbeState` — setup-only probe endpoint.
- `crates/nagent-agents/src/agents/email/` — one file per tool.

## Design

### 1. `ServiceDef` `shortcut` — `crates/nagent-agents/src/shortcut_service.rs`

```rust
pub const SHORTCUT_SERVICE: ServiceDef = ServiceDef {
    id: "shortcut",
    display_name: "Shortcut (app.shortcut.com)",
    icon: …,
    fields: &[
        FieldDef { key: "api_token", kind: FieldKind::Password,
                   required: true, echo_on_edit: ECHO_ON_EDIT_PASSWORD, … },
        FieldDef { key: "username", kind: FieldKind::Text,   // mention_name, filled by the probe
                   required: true, echo_on_edit: false, … },
    ],
};
```

The token is **never** logged or surfaced in an error string
(AGENTS.md §6): the `Shortcut-Token` header is redacted before any
error path, non-2xx bodies are truncated before mapping to
`AgentError::Upstream`.

### 2. Setup probe — `crates/nagent-server/src/credentials/shortcut_probe.rs`

`POST /api/integrations/shortcut/probe` with `{api_token}` →
`GET {base_url}/members/me` → returns `{id, name, mention_name}` and
the UI stores `username = mention_name`. Audit row kind
`shortcut_probe_<outcome>`. Mirrors `email_probe.rs`.

### 3. Shared client — `crates/nagent-agents/src/agents/shortcut/client.rs`

- Requests on `pool.public()` with
  `allowlist = ["api.app.shortcut.com"]` (configurable).
- Headers: `Shortcut-Token: <token>` from
  `ctx.secret("shortcut", "api_token")`, plus
  `User-Agent: nagent/<version>`.
- Error mapping: 401 → "token rejected, reconnect via
  /settings/integrations"; 429 → `AgentError::RateLimited` with the
  `Retry-After` hint (200 req/min quota — the cache matters); 404 →
  not found; other → `Upstream` (truncated body).
- Per-call timeout `[agents.shortcut].timeout_ms` (default 10 s).
- In-process cache keyed by `(user_id, tool, args)`, `cache_ttl_secs`
  default 60 (same pattern as `x_timeline`).

### 4. Config

```toml
[agents.shortcut]
base_url       = "https://api.app.shortcut.com/api/v3"
allowlist      = ["api.app.shortcut.com"]
timeout_ms     = 10_000
max_items      = 200
max_body_bytes = 65_536      # cap on story/epic descriptions the LLM sees
cache_ttl_secs = 60
backlog_query  = "…"         # default search query for shortcut_list_backlog
```

Env: `SHORTCUT_AGENT_BASE_URL`, `SHORTCUT_AGENT_ALLOWLIST`,
`SHORTCUT_AGENT_TIMEOUT_MS`, `SHORTCUT_AGENT_MAX_ITEMS`,
`SHORTCUT_AGENT_MAX_BODY_BYTES`, `SHORTCUT_AGENT_CACHE_TTL_SECS`,
`SHORTCUT_AGENT_BACKLOG_QUERY`.

Triple-definition spots: `config_doc.rs` (agents crate) +
`config/agents.rs` + `config_file.rs` (server crate).

### 5. Registry + cargo wiring

- `shortcut-agent = []` in `crates/nagent-agents/Cargo.toml` (no new
  deps), in `all-agents`; forwarded as
  `shortcut-agent = ["nagent-agents/shortcut-agent"]` in
  `crates/nagent-server/Cargo.toml` + its `all-agents`.
- Ten `AgentDescriptor`s in `AGENT_DESCRIPTORS`, `feature:
  "shortcut-agent"`.
- `app.rs::build_app` appends `SHORTCUT_SERVICE` to the service
  registry slice.
- Inline-approval posture like the other integrations (third-party
  read with user credentials): confirm card, session override
  allowed.
- `llm/prompt.rs`: append the `shortcut_*` block to the
  configured-integrations prompt section.

### 6. Files

New:

- `crates/nagent-agents/src/shortcut_service.rs`
- `crates/nagent-agents/src/agents/shortcut/{mod,client,me,
  list_workflows,list_projects,list_epics,get_epic,list_iterations,
  search_stories,list_backlog,get_story,list_labels}.rs`
- `crates/nagent-server/src/credentials/shortcut_probe.rs`
- `docs/integrations/shortcut.md`
- `crates/nagent-server/tests/shortcut_e2e.rs`

Edited: both `Cargo.toml`s, `agents.rs`, `config_doc.rs`,
`config/agents.rs`, `config_file.rs`, `app.rs`, `http/mod.rs` (probe
mount), `llm/prompt.rs`, `docs/architecture.md` (§2.4, §2.6),
`README.md`, `docs/examples/config.toml.example`.

### 7. Validation

1. `cargo fmt --all`; `cargo build --all-targets --all-features` zero
   warnings; `cargo clippy --all-targets --all-features -- -D warnings`;
   `cargo test --all`; `cargo audit`.
2. Unit tests per tool against a loopback `wiremock` fixture
   (`base_url` override): happy paths, 401, 429 → `RateLimited`,
   body cap, search-query passthrough, backlog default query.
3. Redaction test: failing upstream must never leak the token in the
   error string or captured logs (AGENTS.md §6).
4. Descriptor invariant tests pass with `all-agents`.
5. `tests/shortcut_e2e.rs`: register the token through the `shortcut`
   `ServiceDef` PUT flow, probe, invoke `shortcut_list_backlog`
   against the fixture, assert JSON shape + presence in
   `GET /v1/agents`.
6. Manual smoke with a real API token: connect in Settings →
   Integrations, then chat "what's in my Shortcut backlog? draft a
   plan for the next iteration".

## Out of scope (v1)

- Write operations (create/update story, comments, moves).
- OAuth 2.0 flow (token only).
- Webhooks / real-time sync.
- Documents API (`/docs`) — could be a follow-up (`shortcut_list_docs`).
