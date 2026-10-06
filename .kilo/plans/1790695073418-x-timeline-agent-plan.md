# X Timeline Agent — Implementation Plan

## Goal

Add a server-side chat tool (`x_timeline`) that reads the calling user's
authenticated X (Twitter) home timeline, summarises the recent posts, and
deduplicates posts that share the same theme so the user gets a clean
overview of interesting news. The agent reuses the existing per-user
credentials vault (`nagent_db::Credentials`) to store the user's OAuth
tokens, and uses the existing chat-agent wiring (`AgentRegistry` /
`AGENT_DESCRIPTORS`, `EgressPool`, `tools[]` SSE events) so it slots
into the LLM tool loop without changes to the proxy or the frontend.

This plan is a rewrite of the earlier draft (`1790695073418`). The
architecture has drifted significantly since the draft was written —
see "Drift since the draft" below — so every section that referenced
`crates/stt-server/` or hand-rolled the OAuth dance / credentials
framework has been replaced with the corresponding current surface.

## Locked decisions (from clarifying questions)

These decisions still hold and are the contract for the implementation:

- **Upstream**: X API v2 timeline endpoints
  (`api.x.com/2/users/:id/timelines/reverse_chronological` for
  `following`, `…/for_you` for `for_you`). Bearer token + optional
  refresh token, supplied per-user via the existing credentials vault.
- **Authentication**: OAuth 2.0 with PKCE, scopes
  `tweet.read users.read follows.read`. The flow is server-driven;
  tokens are stored encrypted per-user, exactly like the CalDAV
  integration. PKCE is mandatory; `client_secret` is optional
  (X's "Web App, Confidential Client" mode requires it, "Single Page
  App / Native" mode does not).
- **Account binding**: Strict 1-account-per-nagent-user. The OAuth
  callback stores tokens under the authenticated session user; no
  shared bot account.
- **Timeline mode**: Per-call parameter `mode = "following" | "for_you"`.
  Default `"following"` because it is deterministic and avoids
  promoted content.
- **Dedup**: The agent returns raw, structured posts. The LLM does
  the theme clustering + summary in its final reply (one tool
  round, no extra LLM calls). The agent pre-sorts posts by
  `created_at` descending and surfaces shared
  `entities.urls.expanded_url` so the model can cluster cheaply.
- **Refresh ownership**: The agent itself handles refresh (locked
  decision from the original draft). On 401, the agent POSTs to
  `/2/oauth2/token` and writes the new access_token + new
  refresh_token + new expires_at back to the vault. This requires a
  new write-side capability on `UserContext` — see
  "Capability extension: `SecretSink`" below.

## Drift since the draft

The draft predated several architectural refactors. The relevant
drift:

1. **Crate rename.** `crates/stt-server/` is now `crates/nagent-server/`
   and `lib.rs` no longer has an `auth/x_oauth.rs` module — OAuth
   dance primitives live in their own `nagent-server/src/oauth/`
   tree (`pkce`, `state`, `refresh`). Every file path in the draft
   is stale.
2. **Agent subsystem extracted.** The `Agent` trait, `UserContext`,
   `AgentRegistry`, `AGENT_DESCRIPTORS`, `EgressPool`, and every
   built-in agent implementation live in `crates/nagent-agents/`
   (`nagent-agents/src/agents.rs` + `nagent-agents/src/agents/*`).
   Adding an agent is "one directory in `nagent-agents/`, one
   descriptor, one config struct, one feature line". The draft
   assumed the agent lived in `stt-server/src/agents/`.
3. **Per-user credentials framework already exists.** AES-GCM vault,
   `CredentialResolver`, `CredentialState`, `PUT /api/integrations/...`
   routes, `CredentialError` enum, and `SecretSource` capability trait
   all live in their current locations. The draft described building
   this from scratch.
4. **OAuth dance primitives already exist.** `nagent_server/src/oauth/
   {pkce,state,refresh}.rs` already provide `PkcePair`,
   `StateStore`, `StateStoreEntry`, `RefreshTokenClient` (trait),
   `RefreshTokenError`, and `TokenSet`. The draft described
   re-implementing them.
5. **Static `AGENT_DESCRIPTORS` factory.** New agents are registered
   by appending an `AgentDescriptor` to the static slice in
   `nagent-agents/src/agents.rs` (gated by `#[cfg(feature = "x-agent")]`).
   The draft assumed a hand-rolled `from_config` switch in
   `stt-server/src/agents/mod.rs`.
6. **Cargo features are forwarded.** Each agent has a feature on
   `nagent-agents` (`x-agent`) that is re-exported through
   `nagent-server`'s `x-agent = ["nagent-agents/x-agent"]`. The
   `all-agents` meta-feature must list it on **both** crates or the
   server-side wiring silently degrades to the empty default
   (the same sharp edge that bit the CalDAV work — see the
   `af1b66a fix(build): forward local per-agent features under
   all-agents` commit).
7. **`AppState` composition.** The boot wiring lives in
   `nagent-server/src/app.rs::build_app`, not inline in `main.rs`.
   Auth sub-states (`oidc`, `passkey`, future `x`) sit on
   `AuthState`; the per-user service registry is built inside
   `build_app` from the per-feature constants in `nagent-agents`.
8. **`EgressPool` central policy.** Network agents reuse one of two
   pre-warmed `reqwest::Client` instances (`pool.public()` for
   outbound public hosts, `pool.strict()` for the SSRF-tight
   variant). The X agent uses `pool.public()` because `api.x.com`
   is a public host (with a hostname allow-list to lock down
   exactly that host).
9. **`AuthBackendKind` is `nagent`'s own login backends.**
   `cfg.auth.backends` controls *how the user signs into nagent*
   (password / OIDC / passkey). The X OAuth flow is a per-user
   **integration**, not a nagent login backend — the draft's
   `[auth.x]` section is wrong. The X OAuth config lives at
   top-level `[x_oauth]` (mirroring `[agents.caldav]`); the
   per-runtime agent knobs live at `[agents.x_timeline]`.
10. **Existing reference implementation is CalDAV** (plan
    `1790963194218`). The CalDAV plugin — three chat agents
    (`caldav_list_events` / `caldav_get_event` /
    `caldav_create_event`), one `ServiceDef` (`caldav`), one
    setup-only probe endpoint, one `CalDavAgentConfig` —
    is the template for this plan. Each section below points at
    the CalDAV analogue so the implementer has a working
    example to mirror.

## What changes (in implementation order)

The plan lands in **one commit family** but the commit breakdown
mirrors the order:

1. **Capability** — add `SecretSink` to `UserContext`
   (`nagent-agents/src/agents.rs` + `nagent-server/src/credentials/`)
   so the agent can write back refreshed tokens. Smallest possible
   surface: one trait, one impl, one method on `UserContext`.
2. **OAuth flow** — `nagent-server/src/oauth/x.rs` + a new
   `XOAuthState` mounted on `AuthState`. Reuses the existing
   `PkcePair`, `StateStore`, and `RefreshTokenClient` primitives.
   Routes: `GET /api/auth/login/x/start`, `GET
   /api/auth/login/x/callback` (public subtree, mirroring OIDC
   URLs), `DELETE /api/auth/login/x/disconnect` (protected).
3. **`ServiceDef` `x_account`** — `nagent-agents/src/x_account_service.rs`
   mirroring `caldav_service.rs`. Id `"x_account"`. Six fields:
   `access_token` (Password), `refresh_token` (Password),
   `token_scope` (Text), `x_user_id` (Text), `x_screen_name`
   (Text), `token_expires_at` (Text, RFC 3339). Display-only fields
   surface plaintext in the existing `/api/integrations/:id`
   response; `access_token` + `refresh_token` are masked like the
   CalDAV password field.
4. **New agent `x_timeline`** — `nagent-agents/src/agents/x_timeline.rs`
   mirroring `caldav/list_events.rs`. One agent (read-only),
   `EgressClient` built from `pool.public()` with
   `allowlist=["api.x.com","x.com"]` (configurable).
5. **Cargo wiring** — `x-agent` feature on both crates, listed
   under `all-agents` on both, Makefile picks it up implicitly.
7. **Architecture / integration docs** — new
   `docs/integrations/x.md`, `README.md` + `AGENTS.md` updates,
   `docs/architecture.md` §2.4 / §2.6 / §"Updates to row" updates.

## Affected files

### New files

- `crates/nagent-agents/src/x_account_service.rs` — static
  `X_ACCOUNT_SERVICE: ServiceDef` mirroring `caldav_service.rs`.
- `crates/nagent-agents/src/agents/x_timeline.rs` — the chat agent.
- `crates/nagent-server/src/oauth/x.rs` — `XOAuthState`,
  `start_handler`, `callback_handler`, `disconnect_handler`,
  and the `RefreshTokenClient` impl for X's token endpoint.
- `crates/nagent-server/src/oauth/x_routes.rs` — small
  `build_x_oauth_router(state: Arc<AppState>) -> Router<...>`
  mirroring `credentials/caldav_probe.rs::build_caldav_probe_router`
  (same `ProbeState` phantom-state trick).
- `docs/integrations/x.md` — operator guide mirroring
  `docs/integrations/caldav.md`.
- `crates/nagent-server/tests/x_oauth_e2e.rs` — full
  end-to-end test against a `wiremock` X fixture
  (`/2/oauth2/token`, `/2/users/me`, `/2/users/:id/timelines/...`).

### Edited files

- `crates/nagent-agents/Cargo.toml` — add
  `x-agent = []` (no extra deps; `reqwest` + `secrecy` are already
  unconditional). Add `x-agent` to the `all-agents` list.
- `crates/nagent-agents/src/lib.rs` — re-export
  `x_account_service::X_ACCOUNT_SERVICE`.
- `crates/nagent-agents/src/config.rs` — add
  `pub x_timeline: XTimelineAgentConfig` to `AgentConfigs`; re-export
  `XTimelineAgentConfig` from `agents::config_doc`.
- `crates/nagent-agents/src/agents.rs` — append the `x_timeline`
  `AgentDescriptor` (cfg-gated by `x-agent`) and the
  `#[cfg(feature = "x-agent")] pub mod x_timeline;` line.
- `crates/nagent-agents/src/agents/config_doc.rs` — add
  `XTimelineAgentConfig` (timeout, max_posts, allowlist, cache_ttl_secs,
  base_url) + `Default`.
- `crates/nagent-agents/src/agents.rs` (further) — extend
  `UserContext` with `update_secret(...)` backed by an
  `Arc<dyn SecretSink>`. See "Capability extension: `SecretSink`"
  below for the exact signature.
- `crates/nagent-server/Cargo.toml` — add
  `x-agent = ["nagent-agents/x-agent"]`. Add `x-agent` to the
  `all-agents` list. No new crate dependencies (`reqwest` +
  `serde_json` + `base64` already unconditional; `oauth2` is
  re-exported from `openidconnect` if we ever need it — current
  design uses hand-rolled JSON because the X schema is small).
- `crates/nagent-server/src/lib.rs` — `pub mod oauth;` already
  present; nothing to add at the top level.
- `crates/nagent-server/src/oauth/mod.rs` — re-export
  `pub mod x;` behind `#[cfg(feature = "x-agent")]` plus
  `pub use x::{build_x_oauth_router, XOAuthState};`.
- `crates/nagent-server/src/state.rs` — add `pub x:
  Option<Arc<XOAuthState>>` to `AuthState` (mirrors `pub oidc:
  Option<Arc<OidcState>>`).
- `crates/nagent-server/src/app.rs` — when
  `cfg.x_oauth.client_id != null`, build `XOAuthState` and stash
  it on `auth.x`. Append `X_ACCOUNT_SERVICE` to the per-feature
  service registry slice (the existing `cfg(feature = "caldav-agent")`
  arm sets the precedent).
- `crates/nagent-server/src/auth/router.rs` —
  `build_public_auth_router` mounts `/api/auth/login/x/start` and
  `/api/auth/login/x/callback` when `auth_state.x.is_some()`.
  `build_protected_auth_router` mounts
  `/api/auth/login/x/disconnect` (CSRF-protected, POST) under
  the same `RequireAuth` gate.
- `crates/nagent-server/src/credentials/resolver.rs` — add
  `SecretSinkImpl` (impl `SecretSink` for the existing
  `nagent_db::Credentials` repository, behind `auth.enabled`).
- `crates/nagent-server/src/config.rs` / `config_file.rs` /
  `config/mod.rs` — add `XOAuthConfig` + `AgentsXTimelineConfig`
  structs and TOML mirrors. See "Configuration" below.
- `crates/nagent-server/src/config/auth.rs` — add the
  `x_oauth: XOAuthConfig` field on `AuthConfig` (or, cleaner,
  a new top-level `XOAuthConfig` field on `Config`; see
  "Configuration" for the chosen location).
- `crates/nagent-server/src/llm/prompt.rs` — append the
  `x_timeline` tool description to the configured-integrations
  block, mirroring the CalDAV entry.
- `Makefile` — no edit needed: every `run*` target already uses
  `--features all-agents` and the meta-feature on both crates
  will list `x-agent` once. Operator opt-in is "build with
  `--features nagent-server/x-agent`" (documented in
  `docs/integrations/x.md`).
- `docs/architecture.md` — append `x_timeline` to the agents
  table in §2.4, append `x_account` to §2.6, add a §"Capability:
  SecretSink" subsection under §2.2, and update §"Updates to row"
  with the new docs entry.
- `README.md` — add an "X (Twitter) timeline" section pointing
  to `docs/integrations/x.md`.
- `AGENTS.md` — add a `Constraints_x_oauth_*` block:
  - tokens never logged at any level (Debug/Display impls use
    `SecretString`),
  - PKCE verifier never crosses an HTTP boundary (it lives
    only in `StateStore`'s `payload` JSON, server-side),
  - refresh-token write-back is single-writer (see "Risks").

## Data model: extending the credentials vault

The existing vault — `nagent_db::Credentials` over the
`user_credentials (user_id, service, field_key)` table, encrypted
with AES-GCM — is already flexible enough; we add a new
`ServiceDef` and **six** new field rows per user. The shape:

```text
service = "x_account"
fields:
  - access_token      (Password)   OAuth access_token (~120-char opaque)
  - refresh_token     (Password)   OAuth refresh_token (optional but
                                   recommended)
  - token_scope       (Text)       space-separated scopes from /oauth2/token
  - x_user_id         (Text)       numeric X user id, needed to build
                                   the timeline URL
  - x_screen_name     (Text)       @handle, surfaced in the chat UI
                                   ("Connecté en tant que @naval")
  - token_expires_at  (Text)       RFC 3339 timestamp from
                                   `expires_in` + issued_at
```

`access_token` + `refresh_token` are stored as `Password`-kind
so the UI masks them; the other four fields are display-only
and surfaced plaintext in the existing
`GET /api/integrations/:id` response.

**No schema migration is required** — `user_credentials` is
already keyed by `(user_id, service, field_key)` and
`AgentError::CredentialsMissing` / `AgentError::CredentialsDecryptFailed`
already exist.

## Capability extension: `SecretSink`

The current `UserContext` exposes a read-only `SecretSource`
capability (`nagent-agents/src/agents.rs::secret(...)`). The X
agent's locked-decision "refresh handled by the agent itself"
requires writing back the new `access_token` + `refresh_token` +
`token_expires_at` to the vault after a 401. We add a write-side
capability in the same shape:

```rust
// crates/nagent-agents/src/agents.rs
#[async_trait]
pub trait SecretSink: Send + Sync {
    /// Replace the supplied (service, field) pairs atomically for
    /// `user_id`. The implementation must use the same audit row
    /// kind as `SecretSource` (`credential_access`) so the audit
    /// log reads uniformly across reads and writes.
    async fn update(
        &self,
        user_id: Uuid,
        service: &str,
        fields: &[(&str, secrecy::SecretString)],
    ) -> Result<(), AgentError>;
}
```

And the matching `UserContext::update_secret(...)` method:

```rust
pub async fn update_secret(
    &self,
    service: &str,
    fields: &[(&str, secrecy::SecretString)],
) -> Result<(), AgentError> {
    let Some(sink) = &self.sink else {
        return Err(AgentError::AgentFailed(
            "credential sink not wired in this context".into(),
        ));
    };
    sink.update(self.user_id, service, fields).await
}
```

The `UserContext` struct gains a `sink: Option<Arc<dyn SecretSink>>`
field. Constructors:

- `UserContext::new(...)` — `sink: None` (test-only).
- `UserContext::for_chat_session(...)` — `sink: Some(resolver)`
  when the request goes through the LLM tool loop and the
  per-user credentials framework is enabled.
- `UserContext::for_tests(...)` — `sink: None` (test-only).

The server-side `SecretSink` impl lives at
`crates/nagent-server/src/credentials/resolver.rs` (or a new
`sink.rs`) and reaches `nagent_db::Credentials::upsert` through
the existing scoped `for_user(user_id).credentials()` view. The
implementation must read the existing row, merge the new fields
into it (so a refresh that only knows about `access_token` does
not wipe `refresh_token`), then re-encrypt + upsert — the same
round-trip the OAuth callback uses.

## OAuth 2.0 PKCE flow (server-driven)

The flow reuses the existing `oauth::pkce`, `oauth::state`, and
`oauth::refresh` primitives and adds the X-specific handlers in
`nagent-server/src/oauth/x.rs`. The shape mirrors the existing
OIDC URL surface (`/api/auth/login/x/*` mirrors
`/api/auth/login/oidc/*`).

### `XOAuthState` (mounted on `AuthState`)

```rust
#[derive(Clone)]
pub struct XOAuthState {
    pub cfg: Arc<XOAuthConfig>,
    pub store: nagent_db::Db,                  // for upsert + audit
    pub key: Arc<CredentialsKey>,              // for encrypt
    pub state: oauth::state::StateStore,       // PKCE/state round-trip
    pub http: reqwest::Client,                 // .public() pool
}
```

`app::build_app` builds it when `cfg.x_oauth.client_id` is
non-empty AND `cfg.x_oauth.enabled == true`; the failure mode
is "log warn, leave `auth.x = None`" (mirrors the existing OIDC
build_oidc_state behaviour). The state is then available via
`auth.x.as_ref()` in `auth/router.rs`.

### `GET /api/auth/login/x/start`

1. Authenticated user (route sits in `build_public_auth_router`
   but reads the session cookie set by the auth middleware; the
   route itself does not require an active session — the same
   pattern OIDC uses for its `/callback`).
2. Generate `state` token via `state_store.push(serde_json::json!({
   "user_id": user_id, "pkce_verifier": pair.verifier.as_str(),
   "created_at": now_unix() }))`.
3. Generate `PkcePair::generate()`.
4. 302 redirect to
   `https://x.com/i/oauth2/authorize?response_type=code&client_id={...}&redirect_uri={...}&scope=tweet.read%20users.read%20follows.read&state={state_token}&code_challenge={challenge}&code_challenge_method=S256`.

### `GET /api/auth/login/x/callback`

1. Extract `state` + `code` from the query string.
2. `state_store.pop(&state_token)` — reject on missing / expired
   → redirect to `/settings/integrations?x_error=expired_state`.
3. POST `https://api.x.com/2/oauth2/token` with
   `grant_type=authorization_code`, `code`, `redirect_uri`,
   `client_id`, `code_verifier`. Include `client_secret` when
   configured.
4. Parse the JSON into `access_token`, `refresh_token`,
   `expires_in`, `scope`. Also call `GET /2/users/me` with the
   new bearer to fetch `x_user_id` + `x_screen_name`.
5. Encrypt + UPSERT every field through the same
   `nagent_db::Credentials::upsert(...)` path the existing
   `PUT /api/integrations/:id/credentials` route uses, with the
   same `credential_oauth_x_connected` audit row.
6. Redirect to `/settings/integrations?x_connected=1`.

### `POST /api/auth/login/x/disconnect` (protected)

Authenticated; deletes every `x_account` field for the calling
user via `nagent_db::Credentials::delete_service(...)`. Mirrors
the existing `DELETE /api/integrations/:id/credentials` route.

### Token refresh (executed inside the agent)

When the timeline call returns 401 and the row has a non-empty
`refresh_token`, the agent:

1. POSTs to `/2/oauth2/token` with
   `grant_type=refresh_token`, `refresh_token`, `client_id`,
   `client_secret` (if any).
2. Reads the existing `x_account` fields from the vault via
   `ctx.secret(...)` (the sink is write-only; the source is
   read-only).
3. Builds the new field set: keep `x_user_id` / `x_screen_name`
   / `token_scope`; replace `access_token`, `refresh_token`,
   `token_expires_at` with the refreshed set.
4. Calls `ctx.update_secret("x_account", &[...])` — the
   `SecretSink::update` impl merges + re-encrypts + upserts.
5. Retries the original timeline call once with the new
   bearer.

This is the same `RefreshTokenClient` trait
(`nagent-server/src/oauth/refresh.rs`) the OIDC module will use
(`refresh::on_exchange`); the X impl is
`XRefreshTokenClient { http, cfg, client_id, client_secret }` and
lives in `oauth/x.rs`.

## New agent: `x_timeline`

### Wiring

Cargo feature `x-agent` on both crates. When `x-agent` is on
AND `agents.enabled = true`, the new `AgentDescriptor` entry in
`AGENT_DESCRIPTORS` pushes it alongside the existing 11 (CalDAV
brought the total to 13 once `read_document` is counted). The
`all-agents` meta-feature on both crates lists `x-agent`.

### `name()` / `description()` / `parameters_schema()`

```text
name: "x_timeline"
description: "Read the X (Twitter) home timeline of the user's connected X
   account via the v2 API (mode 'Abonnements' (Following) by default, or 'Pour Vous'
   (For You) on demand). Returns the ~20 most recent posts, pre-sorted newest-first,
   with author, date, text, hashtags, and URLs. Use for 'résume ma timeline X',
   'quelles sont les news intéressantes aujourd'hui sur mon compte', 'déduplique
   les posts qui parlent du même sujet'. Requires the user to have connected their
   X account via /settings/integrations. Read-only — never posts or replies."
parameters_schema:
  mode: enum ("following" | "for_you"), default "following"
  max_posts: integer, 1..=100, default 20
  since_hours: integer, 1..=168, optional — server-side time window
  language: enum ("fr" | "en"), optional — prefer posts whose detected lang matches
```

### `invoke(ctx, args)` outline

1. Resolve the secrets via `ctx.secret("x_account", ...)`:
   `access_token`, `refresh_token`, `x_user_id`, `token_expires_at`,
   `x_screen_name`. Any missing →
   `AgentError::CredentialsMissing { service: "x_account", … }`.
   Decrypt failures → `AgentError::CredentialsDecryptFailed`.
2. If `token_expires_at` is past or within 60 s, refresh (see
   §"Token refresh").
3. Build URL:
   `https://api.x.com/2/users/{x_user_id}/timelines/reverse_chronological`
   (or `…/for_you`) with
   `max_results = max_posts`,
   `tweet.fields = created_at,author_id,public_metrics,entities,lang`,
   `expansions = author_id`,
   `user.fields = username,name`.
4. Single `EgressClient::get(...)` with
   `Authorization: Bearer {access_token}`. Map non-2xx →
   `AgentError::Upstream { status, body }`. 401 → trigger
   refresh + retry once.
5. Parse JSON: posts array + `includes.users[]` for author
   screen names.
6. Project to the result JSON below; sort by `created_at`
   descending; cap at `max_posts`; optionally filter by
   `since_hours`; optionally prefer `lang`.
7. Return as
   `Ok(serde_json::to_string(&json!({ … })))`.

### Result JSON shape

```json
{
  "mode": "following",
  "fetched_at": "2026-09-29T15:00:00Z",
  "count": 17,
  "posts": [
    {
      "id": "1234567890",
      "author": "@naval",
      "author_name": "Naval",
      "created_at": "2026-09-29T14:42:00Z",
      "lang": "en",
      "text": "…",
      "urls":    ["https://example.com/article"],
      "hashtags": ["ai"],
      "mentions": ["@paulg"],
      "metrics": { "likes": 412, "retweets": 88, "replies": 31 }
    }
  ],
  "shared_links": ["https://example.com/article"],
  "hint": "Posts sorted newest-first. Cluster posts that share urls/hashtags/keywords for the theme dedup."
}
```

`shared_links` is the cheap deterministic signal the LLM uses
to start clustering; the rest of the work happens in the LLM's
final reply.

### Caching

A short in-process `Mutex<HashMap<(user_id, mode), (timestamp, payload)>>`
with `cache_ttl_secs` default 60, max 600, prevents hammering
`api.x.com` when the user asks "summarise my timeline" five
times in a row. Keyed by `(user_id, mode)` so different users
or modes do not collide. Optional; turn off with
`cache_ttl_secs = 0`. Cache is per-process (no cross-instance
sharing) and never persisted to disk; if the process restarts
the cache is cold.

## Configuration

Two new top-level sections:

```toml
# [agents.x_timeline] — runtime knobs (mirrors [agents.caldav]).
timeout_ms    = 8000
max_posts     = 20           # LLM-callable upper bound (also enforced server-side)
allowlist     = ["api.x.com", "x.com"]
cache_ttl_secs = 60          # 0 disables the in-process cache
base_url      = "https://api.x.com"

# [x_oauth] — OAuth client + scopes (NOT under [auth.*]; auth.* is
# for nagent's own login backends, the X flow is a per-user
# integration).
enabled        = true       # master switch for /api/auth/login/x/*
client_id      = "..."      # from developer.x.com
client_secret  = ""         # empty = PKCE-only public client
redirect_path  = "/api/auth/login/x/callback"  # combined with public_url
scopes         = ["tweet.read", "users.read", "follows.read"]
timeout_ms     = 8000       # for /oauth2/token + /users/me + timeline
```

Env equivalents (matching the existing `env > TOML > default`
precedence):

| TOML field | Env var |
| --- | --- |
| `agents.x_timeline.timeout_ms` | `X_TIMELINE_TIMEOUT_MS` |
| `agents.x_timeline.max_posts` | `X_TIMELINE_MAX_POSTS` |
| `agents.x_timeline.allowlist` | `X_TIMELINE_ALLOWLIST` |
| `agents.x_timeline.cache_ttl_secs` | `X_TIMELINE_CACHE_TTL_SECS` |
| `agents.x_timeline.base_url` | `X_TIMELINE_BASE_URL` |
| `x_oauth.enabled` | `X_OAUTH_ENABLED` |
| `x_oauth.client_id` | `X_OAUTH_CLIENT_ID` |
| `x_oauth.client_secret` | `X_OAUTH_CLIENT_SECRET` |
| `x_oauth.redirect_path` | `X_OAUTH_REDIRECT_PATH` |
| `x_oauth.scopes` | `X_OAUTH_SCOPES` |
| `x_oauth.timeout_ms` | `X_OAUTH_TIMEOUT_MS` |

## Makefile / build

No edit needed — every `run*` target already uses
`--features all-agents`. Operator opt-in is:

```bash
cargo build -p nagent-server --features x-agent
cargo build -p nagent-server --features all-agents   # includes x-agent
```

`x-agent` is independent of the GPU backend features and of
`real-backend`; an operator can ship a CPU build with X
enabled just by listing the feature once.

## Failure modes (must surface to the LLM as actionable tool errors)

| Condition | Surfaced as |
|---|---|
| User has not connected X | `AgentError::CredentialsMissing { service: "x_account", field: "access_token" }` — chat UI prompts to connect. |
| Access token expired + refresh failed | `AgentError::AgentFailed("X OAuth refresh failed: {reason}")` with a hint "reconnect X via /settings/integrations". |
| X returns 401 even after refresh | Same as above; never loop forever. |
| X returns 429 (rate limit) | `AgentError::Upstream { status: 429, body: <truncated> }` — LLM can suggest "réessayer dans quelques minutes". |
| X returns 5xx | `AgentError::Upstream { status, body }`. |
| Network / TLS error | `AgentError::AgentFailed("connect/read failed: {err}")`. |
| Malformed X JSON | `AgentError::AgentFailed("X response parse: {err}")`. |
| OAuth `state` mismatch / expired | Redirect with `?x_error=expired_state`; never reveals the verifier. |
| OAuth token exchange 4xx | Redirect with `?x_error=token_exchange_failed`; log the body at WARN, never the access_token. |

## Capability extension: `SecretSink` — error contract

| Condition | Surfaced as |
|---|---|
| `SecretSink` not wired (test / direct-invoke path) | `AgentError::AgentFailed("credential sink not wired in this context")` — the agent should fall back to "ask the user to reconnect X". |
| Encrypt failure on the new field set | `AgentError::AgentFailed("X refresh write encrypt failed: {err}")`. |
| UPSERT DB failure | `AgentError::AgentFailed("X refresh write DB error: {err}")`. |

## Validation plan

1. `cargo fmt --all` + `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
2. `cargo test --workspace --all-features` —
   - `nagent-agents/src/agents/x_timeline.rs::tests` — URL
     builder, post projection, dedup hint extraction, refresh
     path against a `wiremock` fixture.
   - `nagent-server/src/oauth/x.rs::tests` — PKCE/state
     round-trip, `start` + `callback` against a `wiremock` X
     fixture, `disconnect` clears every field.
   - `nagent-server/tests/x_oauth_e2e.rs` — full end-to-end
     against `wiremock`: connect, refresh, disconnect.
3. `cargo test --workspace --features all-agents,test-util` —
   ensure the new `AgentDescriptor` table still satisfies
   `every_descriptor_id_is_unique`,
   `every_descriptor_builds_an_agent_with_matching_id`, and
   `descriptor_table_is_nonempty_when_any_feature_is_on`
   (the existing invariant tests in `nagent-agents/src/agents.rs`).
4. Manual end-to-end with `make run-llm`:
   - Register an X Developer app (Free tier OK; PKCE without
     `client_secret`).
   - Set `X_OAUTH_CLIENT_ID` and `X_OAUTH_REDIRECT_PATH` to
     `http://localhost:11434/api/auth/login/x/callback`.
   - Open `/settings/integrations`, click "Connect X",
     complete the OAuth dance.
   - In the chat view, ask: *"résume ma timeline X aujourd'hui
     et regroupe les posts qui parlent du même sujet"*.
   - Verify: 1 X API call per agent invocation (visible in
     tracing), response is structured, LLM reply is grouped by
     theme.
   - Ask again 30 s later → cache hit (no X API call, visible
     in tracing as `cache hit`).
   - Manually revoke from `x.com` → next call refresh-fails
     and surfaces the reconnect hint.
5. Run a 2-minute `curl` loop against
   `/v1/agents/x_timeline/invoke` to confirm rate-limit handling
   does not panic or deadlock.

## Out of scope (deliberately)

- Posting tweets (no `tweet.write` scope).
- Searching tweets by query / hashtag.
- Reading individual tweet replies / thread continuations.
- Multi-account-per-nagent-user (a second X account would
  clobber the first; explicit decision to keep
  1-account-binding per the locked decisions).
- Sharing the timeline across multiple nagent users (each user
  has their own OAuth tokens, no shared bot account).
- Persisting the agent's response cache to disk across
  restarts (in-process only).
- X API v1.1 endpoints.

## Risks

- **X API policy drift**: X has changed scopes, endpoints, and
  pricing repeatedly. The plan pins to v2 endpoints documented
  as of 2026-09. A future policy change may require
  re-registering the app or adding the `media.fields=…`
  expansion back. Mitigation: keep the URL builder + scope
  list in one file (`x_timeline.rs`) so a future change is a
  single-file edit.
- **PKCE-only flow without client_secret**: X allows PKCE
  without `client_secret` only for apps registered as "Native"
  or "Single Page App" on `developer.x.com`. The
  `docs/integrations/x.md` README spells this out so an
  operator registering a "Web App, Confidential Client" type
  does not silently break the flow.
- **Token replay across processes**: If two server instances
  both attempt to refresh an expired token at the same
  instant, X issues two new refresh tokens; the loser's write
  is discarded. Mitigation: documented single-writer
  assumption; cluster deployments should run the OAuth route
  through a single writer (out of scope for this plan; flagged
  in `AGENTS.md` as a future-work item).
- **X rate limits**: Free / Basic tier is tight (~1500
  reads/user/month). The cache + `max_posts=20` default keeps
  us well under, but `docs/integrations/x.md` documents this
  so an operator does not get surprised by a bill.
- **SecretSink adds a write-side capability** that no other
  agent currently needs. Mitigation: the trait has one method
  (`update`), the server-side impl lives next to the existing
  `CredentialResolver` so the audit-row invariants (`credential_access`
  on every successful write) stay colocated, and the
  `UserContext::sink` field is `None` outside the chat-session
  constructor so the test / direct-invoke paths stay
  sink-free.