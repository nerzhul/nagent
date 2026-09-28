# Plan: Per-User Credentials Framework

## Goal

Add a generic **per-user credentials** subsystem to `stt-server` so any future chat agent can read hostname / login / password configured by the user for a given integration (IMAP, CalDAV, Home Assistant, GitHub, …). Three verticals are scoped but not implemented in this PR — only the framework:

1. **Productivity** (IMAP/SMTP email, CalDAV calendar, Todoist-style tasks).
2. **Dev / knowledge-worker** (GitHub/GitLab, Jira, Linear, Notion, RSS).
3. **Home automation / IoT** (Home Assistant, MQTT).

Each vertical lands in its own follow-up PR and only needs to (a) add a `ServiceDef` to the registry and (b) implement an agent that resolves its creds via `UserContext`. No further framework work.

---

## Design decisions (locked)

| Topic | Choice |
|---|---|
| Storage at rest | AES-256-GCM, server-side key from `AUTH_CREDENTIALS_KEY` env var (64 hex chars, 32 bytes). Refuse to boot when needed. |
| Trait evolution | `Agent::invoke(&self, ctx: &UserContext, args)` — extend in place, every existing agent updates with `let UserContext { .. } = ctx;` no-op. |
| Secret lifetime in memory | `secrecy::SecretString`. Decrypted on demand from DB. `SecretCache` is **per-request only** (DashMap inside `UserContext`, zeroized on drop at end of request). No long-lived plaintext cache. |
| Service catalogue | Static Rust `ServiceRegistry` (one `pub const` list in `crates/stt-server/src/agents/services.rs`). Fronted by `GET /api/integrations*`. |
| Audit | Every successful read → `auth_events` row with `kind = "credential_access"`. Missing → `"credential_missing"`. Decrypt fail → `"credential_decrypt_failed"`. Secret values never logged. New column `target_service TEXT` added via migration `0002_credentials.sql`. |
| Fallback to server-wide creds | Out of scope for this PR. The resolver is per-user only; a future PR can add a server-wide fallback tier for shared agents (weather, etc.). |
| Boot fail-fast | `AUTH_CREDENTIALS_KEY` required when `auth.enabled && agents.enabled && au moins un agent per-user est compilé`. Empty otherwise (single-user trust boundary unchanged). |

---

## Schema (`crates/stt-server/migrations/0002_credentials.sql`)

```sql
-- 0002_credentials.sql — per-user credentials vault + audit column.

-- Audit: extend the existing `auth_events` table so we can correlate
-- credential reads with the integration that triggered them. Backfilled
-- as NULL on existing rows. No secret values ever land in this column.
ALTER TABLE auth_events ADD COLUMN target_service TEXT;

-- user_credentials: one row per (user, service, field). The plaintext
-- is never stored: `nonce` (12 random bytes) + `ciphertext` (aes-gcm
-- output, includes the 16-byte tag suffix that aes-gcm appends). FK
-- cascade on `users.id` so deleting a user wipes their creds.
--
-- The unique constraint on (user_id, service_id, field_key) is what
-- makes the PUT-then-replace-all pattern safe: a second PUT starts
-- from scratch on every call (see `set_service_credentials`).
CREATE TABLE user_credentials (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    service_id      TEXT NOT NULL,
    field_key       TEXT NOT NULL,
    nonce           BLOB NOT NULL,
    ciphertext      BLOB NOT NULL,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (user_id, service_id, field_key)
);

CREATE INDEX user_credentials_user_service_idx
    ON user_credentials(user_id, service_id);
```

The migration runs through the existing `AuthStore::migrate()` path so sqlite + postgres stay in sync.

---

## Module layout

New module `crates/stt-server/src/credentials/` with:

- `mod.rs` — re-exports + top-level error type.
- `key.rs` — `CredentialsKey` (zeroized `Secret<[u8; 32]>`); `from_env()` reads `AUTH_CREDENTIALS_KEY`, validates hex+length, returns the typed key.
- `crypto.rs` — `seal(&key, plaintext) -> EncryptedSecret { nonce, ciphertext }`, `open(&key, &EncryptedSecret) -> SecretString`. Thin wrappers around `aes-gcm::Aes256Gcm`. Constant-time tag comparison is the crate's default.
- `cache.rs` — `SecretCache` (DashMap keyed by `(service_id, field_key)`); `Drop` impl zeroizes values via `SecretString::zeroize()`. One cache per `UserContext`, lifetime = one LLM tool round / one direct invoke.
- `resolver.rs` — `CredentialResolver`; async `get(user_id, service, field) -> Result<Option<SecretString>, CredentialError>`. Holds an `Arc<AuthStore>` + `CredentialsKey`. Writes the audit row on every call (success or miss).

Existing `auth/store.rs` gains three methods (one `match` arm per backend):

```rust
pub async fn upsert_user_credentials(
    &self, user_id: Uuid, service_id: &str,
    fields: &[(String, String)],  // (field_key, plaintext)
) -> Result<(), AuthError>;

pub async fn delete_service_credentials(
    &self, user_id: Uuid, service_id: &str,
) -> Result<u64, AuthError>;

pub async fn list_configured_field_keys(
    &self, user_id: Uuid, service_id: &str,
) -> Result<Vec<String>, AuthError>;
```

`upsert_user_credentials` runs inside a single transaction so partial writes never leave a service half-configured (delete-by-service then batch-insert).

---

## Service registry (`crates/stt-server/src/agents/services.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind { Text, Password, Url }

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub key: &'static str,        // e.g. "host"
    pub label: &'static str,      // e.g. "IMAP server hostname"
    pub kind: FieldKind,
    pub required: bool,
    pub help: Option<&'static str>,
    pub placeholder: Option<&'static str>,
}

#[derive(Debug)]
pub struct ServiceDef {
    pub id: &'static str,                // "email_imap", "caldav", …
    pub display_name: &'static str,
    pub icon: &'static str,              // unicode emoji or short token
    pub fields: &'static [FieldDef],
    pub docs_url: Option<&'static str>,
}

pub struct ServiceRegistry {
    services: &'static [ServiceDef],
}

impl ServiceRegistry {
    pub const fn new(services: &'static [ServiceDef]) -> Self { … }
    pub fn list(&self) -> &[ServiceDef] { … }
    pub fn get(&self, id: &str) -> Option<&'static ServiceDef> { … }
}
```

In v1 the registry ships **empty** (`const EMPTY: &[ServiceDef] = &[];`). Follow-up PRs add entries. The end-user UX is therefore "no integrations available yet" — but every supporting surface (UI section, routes, audit, encryption) is already wired and tested, so adding a new service is just data + an agent.

---

## Agent trait evolution (`crates/stt-server/src/agents/`)

```rust
// user_context.rs
pub struct UserContext {
    user_id: Uuid,
    services: Arc<ServiceRegistry>,
    resolver: Arc<CredentialResolver>,
    cache: Mutex<SecretCache>,        // per-request, zeroized on drop
}

impl UserContext {
    pub fn new(user_id: Uuid, services: Arc<ServiceRegistry>,
               resolver: Arc<CredentialResolver>) -> Self { … }
    pub async fn secret(&self, service: &str, field: &str)
        -> Result<Option<SecretString>, AgentError>;
    pub fn service_def(&self, id: &str) -> Option<&'static ServiceDef>;
    pub fn user_id(&self) -> Uuid;
}

impl Drop for UserContext {
    fn drop(&mut self) { self.cache.lock().expect("poisoned").zeroize(); }
}

#[async_trait]
pub trait Agent: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    async fn invoke(&self, ctx: &UserContext, args: Value)
        -> Result<String, AgentError>;
}
```

All 8 existing agents (`web_fetch`, `datetime`, `weather`, `stock`, `calculate`, `unit_convert`, `wikipedia`, `dictionary`) gain `ctx: &UserContext` as the first parameter and ignore it via `_ctx: &UserContext`. Their existing tests pass through unchanged (the trait method is mocked at the registry level).

`AgentRegistry::from_config` also takes an `Arc<ServiceRegistry>` and the resolver so `run_tool_loop` / `agent_invoke` can construct a fresh `UserContext` per call.

---

## API surface (`crates/stt-server/src/credentials/routes.rs`)

All routes are mounted under the existing `RequireAuth` middleware; the credential owner is always the session user.

| Method | Path | Body | Response |
|---|---|---|---|
| `GET` | `/api/integrations` | – | `{ "data": [{ "id", "display_name", "icon", "fields": [key+label+kind+required+help+placeholder], "configured": bool, "docs_url" }, …] }`. `configured` is true iff every required field is filled for the current user. |
| `GET` | `/api/integrations/:id` | – | Same shape, single service. 404 if unknown id. |
| `PUT` | `/api/integrations/:id/credentials` | `{ "fields": { "<field_key>": "<plaintext>", … } }` | `204 No Content`. Rejects unknown `field_key` with 400. Rejects unknown `:id` with 404. Writes nothing on partial failure (atomic replace). |
| `DELETE` | `/api/integrations/:id/credentials` | – | `204 No Content`. Clears all fields of the service for the current user. |

GET responses **never** carry plaintext values; the boolean per-field `filled` flag is computed server-side from `list_configured_field_keys`. CSRF token required on PUT/DELETE (matches existing `auth` subtree).

---

## UI (`crates/stt-server/src/static/`)

- Add a new `<details id="chat-integrations">` inside the existing `chat-advanced` drawer in `index.html`, after the timezone block. Each row is one service with name + icon + status pill ("Configured" green / "Not configured" grey) and a "Configure" / "Edit" button.
- New module `integrations.js` (loaded by `app.js`) that:
  - `GET /api/integrations` on drawer-open, renders the list.
  - Clicking Configure opens a modal with a `<form>` per `FieldDef`. Password fields use `<input type="password">`. URLs use `<input type="url">`.
  - On submit: `PUT /api/integrations/:id/credentials`, refresh row, close modal.
  - On success: emit a `chat` toast "Credentials saved for `<service>`".
- A chat-banner helper that detects `AgentError::CredentialsMissing { service }` in the SSE stream and renders "Configure `<service>` to use this integration" with a click-to-open-modal link. The error variant is added by the resolver and converted by `llm.rs` before emission.

No `localStorage` involvement: every cred lives server-side.

---

## LLM system prompt

`llm_prompt::inject_default_system_prompt` gains a new opt-in block listing the configured services for the current user. Marker prefix follows the existing pattern (e.g. `The user has the following integrations configured:`). New config knob `LLM_ALLOW_INTEGRATIONS_LIST` (default `true`) with a matching `strip_integrations_list_if_disabled` helper, mirroring the location/timezone kill-switches.

The LLM therefore only sees integrations it can actually call. If the user has not configured `email_imap`, the LLM is not told it exists and will not invent fake credentials.

---

## Boot wiring (`crates/stt-server/src/main.rs`, `lib.rs`, `auth/boot.rs`)

1. `Config::auth.credentials.key_env` (default `"AUTH_CREDENTIALS_KEY"`) — name only, value comes from env.
2. After `AuthStore::connect` succeeds, if `agents.enabled && any per-user agent is registered`:
   - read the env var;
   - decode hex → 32 bytes;
   - build `CredentialsKey` (zeroized);
   - stash in `AppState` as `Arc<CredentialsKey>`.
3. If a per-user agent is registered but the env var is missing/invalid → `ConfigError::CredentialsKeyMissing` → exit 1.
4. `ServiceRegistry::empty()` is the v1 default; an optional `[integrations]` TOML section could later allow sites to override which services are exposed (out of scope here).

`/api/integrations*` routes are mounted in `auth/router.rs::build_protected_auth_router` so they inherit `RequireAuth`.

---

## Concrete agents — out of scope, follow-up PRs

This PR adds the framework only. Per the user's instruction, picking the actual services and agents happens later, one PR per vertical. The pattern each will follow:

1. Add `ServiceDef`s to `agents/services.rs::DEFAULT_SERVICES`.
2. Implement an agent whose `invoke` reads creds via `ctx.secret("svc", "host")` etc.
3. Cargo feature gate (`email-agent`, `caldav-agent`, `homeassistant-agent`, …) wired into `from_config` + the existing `Makefile run*` targets.
4. Wire `make run*` Makefile targets to compile the new features (mirror existing agent features).

The first concrete PR after this one should be the productivity vertical (email + calendar), because the user's "tools du quotidien" framing puts that first.

---

## Validation

- **Migrations** — `cargo test -p stt-server --test auth_migrations` runs the up-migration on a fresh sqlite + postgres, asserts both tables exist, asserts `auth_events.target_service` column exists and is NULL-backfilled.
- **Crypto round-trip** — `seal` then `open` returns the original; tampered ciphertext fails `open`; wrong key fails `open`; both errors mapped to `CredentialError::DecryptFailed`.
- **Resolver** — unit tests cover happy path, missing service, missing field, decrypt failure, audit row written on every call (kind, target_service, user_id correct).
- **Cache zeroize** — `SecretCache::drop` test uses a `core::sync::ExclusiveCell` / volatile read pattern to assert the bytes are zeroized after drop (test-only helper).
- **Trait evolution** — every existing agent test compiles unchanged; new `UserContext::for_tests()` helper produces a no-op ctx for tests that don't care about creds.
- **Routes** — integration test boots an `AppState` against sqlite, registers a dummy service def + a dummy agent that calls `ctx.secret(...)`, PUTs credentials via the route, invokes the agent, asserts the audit row + the agent received the right plaintext (via test-only callback).
- **UI** — manual smoke (recorded for `docs/screenshots/`); no playwright suite in this PR.

---

## Risks & follow-ups

- **Key rotation** — not supported in v1. `AUTH_CREDENTIALS_KEY` is loaded once at boot. Rotation requires re-encrypting all rows; deferred.
- **Server-wide fallback creds** — not in v1. Agents that today take `WEATHER_API_KEY` keep their global config; per-user override is a separate PR.
- **`SecretString` send-ability** — `Secret<String>` is not `Send` across some boundaries by default; verify the chosen version of `secrecy` allows it (it does in current crates.io). If a compile error surfaces, switch to wrapping in `Arc<SecretString>` at the cache boundary.
- **CSRF on PUT** — already covered by the existing `RequireAuth` middleware for state-changing routes, but double-check the `requires_csrf_check` table includes `PUT`.
- **Toml migration of `auth_events.target_service`** — sqlx may complain about the column add for queries that already select `*` from `auth_events`. Grep + retarget those queries to an explicit column list.
