# GitHub agent — read the user's account (repos, issues, PRs, notifications, files) so the LLM can build plans from them

Status: proposal, written 2026-11-25 (step 3 of the session). No code
changes yet. Supersedes item **B6** of
`1791234616248-ux-feature-agent-catalog.md`.

## Goal

A `github-agent` cargo feature that adds a per-user **GitHub**
integration (personal access token in the existing credentials vault)
and a set of **read-only** chat tools:

| Tool | GitHub endpoint | Args |
| --- | --- | --- |
| `github_me` | `GET /user` | — |
| `github_list_repos` | `GET /user/repos` or `GET /users/{owner}/repos` | `owner?`, `type?` (all/owner/member), `sort?` (updated/pushed/full_name), `limit?` (default 30, cap 100) |
| `github_get_repo` | `GET /repos/{owner}/{repo}` | `owner`, `repo` |
| `github_list_issues` | `GET /repos/{owner}/{repo}/issues` | `repo` (as `owner/repo`), `state?` (open/closed/all), `labels?`, `limit?` |
| `github_get_issue` | `GET /repos/{owner}/{repo}/issues/{n}` | `repo`, `number` |
| `github_list_prs` | `GET /repos/{owner}/{repo}/pulls` | `repo`, `state?`, `limit?` |
| `github_get_pr` | `GET /repos/{owner}/{repo}/pulls/{n}` | `repo`, `number` (body included, capped) |
| `github_list_notifications` | `GET /notifications` | `unread_only?`, `limit?` |
| `github_search_issues` | `GET /search/issues` | `query` (GitHub search syntax, e.g. `author:@me is:open`), `limit?` |
| `github_read_file` | `GET /repos/{owner}/{repo}/contents/{path}` | `repo`, `path`, `ref?` (branch/tag/sha) — base64-decoded, capped |

All tools are read-only; write tools (`github_create_issue`,
`github_create_pr`, `github_comment`) are deliberately **out of scope
for v1** (they would be confirm-on-write like `caldav_create_event`,
but the user asked for account *reading* first).

The intended workflow: the user asks "review my GitHub account and
propose a plan for next week" — the LLM calls `github_me` →
`github_list_repos` → `github_search_issues` / `github_list_prs` →
`github_read_file` on the relevant repos, then answers with a plan.

## Patterns to mirror (all landed 2026-11)

- `crates/nagent-agents/src/x_account_service.rs` — `ServiceDef`
  field layout (`Password` kind for the token, `echo_on_edit` flags).
- `crates/nagent-agents/src/agents/x_timeline/mod.rs` — how an agent
  reads vault secrets via `ctx.secret(service, field)`, calls the
  shared egress pool, refreshes/caches, and maps errors.
- `crates/nagent-server/src/credentials/email_probe.rs` +
  `crate::probe::ProbeState` — setup-only probe endpoint pattern.
- `crates/nagent-agents/src/agents/email/` — one file per tool under
  `agents/email/`; same layout under `agents/github/`.

## Design

### 1. `ServiceDef` `github` — `crates/nagent-agents/src/github_service.rs`

```rust
pub const GITHUB_SERVICE: ServiceDef = ServiceDef {
    id: "github",
    display_name: "GitHub",
    icon: "🐙" (or the GitHub mark),
    fields: &[
        FieldDef { key: "personal_access_token", kind: FieldKind::Password,
                   required: true, echo_on_edit: ECHO_ON_EDIT_PASSWORD, … },
        FieldDef { key: "username", kind: FieldKind::Text,
                   required: true, echo_on_edit: false, … }, // filled by the probe
    ],
};
```

Token scopes (documented in `docs/integrations/github.md`):
`read:user` + `repo` (for private repos) — classic PAT, fine-grained
PATs also work. The token is **never** logged or surfaced in error
strings (AGENTS.md §6): the client maps every non-2xx to
`AgentError::Upstream { status, body }` with the body truncated, and
the request builder redacts the `Authorization` header before any
error path (same `redact_command`-style rule as IMAP).

### 2. Setup probe — `crates/nagent-server/src/credentials/github_probe.rs`

`POST /api/integrations/github/probe` with body
`{personal_access_token}` → calls `GET https://api.github.com/user`
with the supplied token → returns `{login, name, avatar_url, plan}`
and the UI stores `username = login`. Audit row kind
`github_probe_<outcome>`, token never stored in the audit row.
Mirrors `email_probe.rs` / `caldav_probe.rs` (`ProbeState<C>`).

### 3. Shared client — `crates/nagent-agents/src/agents/github/client.rs`

- Builds each request on `pool.public()` with
  `allowlist = ["api.github.com"]` (configurable via
  `[agents.github].allowlist`).
- Headers: `Authorization: Bearer <token>` (from
  `ctx.secret("github", "personal_access_token")`),
  `Accept: application/vnd.github+json`,
  `X-GitHub-Api-Version: 2022-11-28`, `User-Agent: nagent/<version>`
  (GitHub rejects requests without a User-Agent).
- Error mapping: 401 → `AgentError::CredentialsDecryptFailed`-style
  "token rejected, reconnect via /settings/integrations" (no
  auto-refresh — PATs don't expire unless set to); 403/429 with
  `X-RateLimit-Remaining: 0` → `AgentError::RateLimited` with the
  `Retry-After` / reset hint; 404 → not found; other → `Upstream`.
- Per-call timeout from `[agents.github].timeout_ms` (default 10 s).
- Short in-process cache keyed by `(user_id, tool, args)` with
  `cache_ttl_secs` default 60 (GitHub PAT quota is 5 000 req/h; the
  same pattern as `x_timeline`).

### 4. Config

One shared agent section (triple-definition spots:
`config_doc.rs` + `config/agents.rs` + `config_file.rs`):

```toml
[agents.github]
timeout_ms     = 10_000
max_items      = 100        # hard cap on limit for every list tool
max_body_bytes = 65_536     # cap on issue/PR/file bodies the LLM sees
cache_ttl_secs = 60         # 0 disables
allowlist      = ["api.github.com"]
base_url       = "https://api.github.com"   # overridable for tests / GHES
```

Env: `GITHUB_AGENT_TIMEOUT_MS`, `GITHUB_AGENT_MAX_ITEMS`,
`GITHUB_AGENT_MAX_BODY_BYTES`, `GITHUB_AGENT_CACHE_TTL_SECS`,
`GITHUB_AGENT_ALLOWLIST`, `GITHUB_AGENT_BASE_URL`.

### 5. Registry + cargo wiring

- `github-agent = []` feature in `crates/nagent-agents/Cargo.toml`
  (no new deps — `reqwest` + `serde_json` are unconditional), listed
  in `all-agents`; forwarded as
  `github-agent = ["nagent-agents/github-agent"]` in
  `crates/nagent-server/Cargo.toml` and added to its `all-agents`.
- Ten `AgentDescriptor` entries appended to `AGENT_DESCRIPTORS` in
  `crates/nagent-agents/src/agents.rs`, all
  `feature: "github-agent"`.
- `app.rs::build_app` appends `GITHUB_SERVICE` to the per-feature
  service registry slice (same `cfg!(feature = …)` arm pattern as
  CalDAV/X/email).
- All tools are read-only but still hit a third-party with user
  credentials → same inline-approval posture as the other
  integrations (confirm card, session override allowed).
- `llm/prompt.rs`: append the `github_*` block to the
  configured-integrations prompt section, mirroring the CalDAV entry.

### 6. Files

New:

- `crates/nagent-agents/src/github_service.rs`
- `crates/nagent-agents/src/agents/github/{mod,client,me,list_repos,
  get_repo,list_issues,get_issue,list_prs,get_pr,list_notifications,
  search_issues,read_file}.rs`
- `crates/nagent-server/src/credentials/github_probe.rs`
- `docs/integrations/github.md`
- `crates/nagent-server/tests/github_e2e.rs`

Edited: both `Cargo.toml`s, `agents.rs` (descriptors + `pub mod`),
`config_doc.rs`, `config/agents.rs`, `config_file.rs`, `app.rs`,
`http/mod.rs` (probe mount), `llm/prompt.rs`, `docs/architecture.md`
(§2.4 agent table + §2.6 services), `README.md` config matrix,
`docs/examples/config.toml.example`.

### 7. Validation

1. `cargo fmt --all`; `cargo build --all-targets --all-features` zero
   warnings; `cargo clippy --all-targets --all-features -- -D warnings`;
   `cargo test --all`; `cargo audit`.
2. Unit tests per tool against a loopback fixture (override
   `base_url` to a `wiremock` server): happy path, 404, 401,
   403-rate-limit (assert `RateLimited` + hint), body cap, base64
   file decode, search query passthrough.
3. Redaction test: with a deliberately failing upstream, assert the
   token string never appears in the returned error or in captured
   log output (AGENTS.md §6).
4. Descriptor invariant tests (`every_descriptor_id_is_unique`, …)
   pass with `all-agents`.
5. `tests/github_e2e.rs`: boot test app, register the PAT through
   the `github` `ServiceDef` PUT flow, probe, then
   `POST /v1/agents/github_list_repos/invoke` against the loopback
   fixture; assert JSON shape and that `GET /v1/agents` lists the ten
   tools.
6. Manual smoke with a real PAT: connect in Settings → Integrations,
   then in chat: "what's open on my GitHub account right now, and
   draft a plan for the week" — expect the LLM to chain
   `github_me` / `github_search_issues` / `github_list_prs` /
   `github_read_file`.

## Out of scope (v1)

- Write operations (create issue / PR / comment / review) — follow-up
  plan, confirm-on-write.
- GitHub OAuth app flow (the X-style dance) — PAT only, like the
  email integration; OAuth can come later as a separate `ServiceDef`.
- GitHub App / fine-grained permission automation beyond the docs.
- GraphQL API, code search endpoint (`/search/code` needs extra
  headers on some PATs — REST `search/issues` is enough for planning).
- Webhooks / real-time notifications.
