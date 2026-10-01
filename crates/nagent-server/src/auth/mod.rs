//! `auth/` — multi-user authentication & user identity .
//!
//! Always compiled in (no cargo feature gate). When the runtime
//! config sets `auth.enabled = false` the server keeps the pre-
//! single-user trust boundary; the modules in this tree still exist
//! but the `auth::boot::auto_bootstrap` function short-circuits and
//! the HTTP routes (registered in `http::build_router`) are guarded
//! by the runtime flag.
//!
//! ## Sub-modules
//!
//! - [`mod@boot`] — auto-bootstrap invoked by `main.rs` (migrate +
//! optional sqlite first-admin creation).
//! - [`mod@store`] — DB-agnostic data access. The public type is
//! [`store::AuthStore`] which is a `match`-dispatched enum over
//! the sqlite / postgres engines; each variant now holds the
//! per-domain repositories from [`crate::db`] .
//! - [`mod@session`] — cookie bearer parsing, session row type,
//! CSRF token generator, `Set-Cookie` builder.
//! - [`mod@middleware`] — `require_auth_middleware` axum middleware
//! that resolves a session into an [`AuthUser`] extension.
//! - [`mod@password`] — argon2id wrapper the `local` backend
//! (`POST /api/auth/login/password`).
//! - [`mod@oidc`] — OIDC backend (`/api/auth/login/oidc/{start,callback}`).
//! - [`mod@passkey`] — WebAuthn backend (`/api/auth/login/passkey/{start,finish}`).
//! - [`mod@routes`] — `/api/me`, `/api/auth/logout`, plus the
//! registration endpoints guarded by `RequireAuth`.
//! - [`mod@login_rate_limit`] — login-attempt rate-limit
//! (DashMap, `(email, ip)` key).
//!
//! The `stt-server auth {create-admin,list-users,delete-user}` CLI
//! subcommand lives in [`crate::cli::auth`].
//!
//! ## Topology
//!
//! ```text
//! HTTP request ──► require_auth_middleware ──► AuthUser extension
//! │
//! └─► session.rs (cookie/bearer/CSRF)
//! └─► store.rs (AuthStore facade)
//! └─► crate::db::{users,sessions,passkeys,…}
//! ```

pub mod boot;
pub mod error;
pub mod login_rate_limit;
pub mod middleware;
pub mod oidc;
pub mod passkey;
pub mod password;
pub mod router;
pub mod routes;
pub mod session;

pub use crate::state::AuthState;
pub use error::AuthError;
pub use error::{require_auth_store, require_oidc_state, require_passkey_state};
pub use nagent_db::UserPreferences;
pub use oidc::OidcState;
pub use passkey::PasskeyState;
pub use session::{AuthUser, SessionRecord};

/// Translate a server-side [`AuthConfig`] into the DB-layer
/// [`nagent_db::DbOptions`]. Keeps the DB crate free of TOML /
/// env / CLI concerns — the server is the only component that
/// knows about those.
impl From<&crate::config::AuthConfig> for nagent_db::DbOptions {
    fn from(cfg: &crate::config::AuthConfig) -> Self {
        // `connect` rejects unknown engines with a clear error
        // message, so we fall back to sqlite here purely to keep
        // `DbOptions` constructible for every input (the actual
        // connect call surfaces the real failure).
        let backend =
            nagent_db::DbEngine::parse(&cfg.db.backend).unwrap_or(nagent_db::DbEngine::Sqlite);
        Self {
            backend,
            url: cfg.db.url.clone(),
            max_connections: cfg.db.max_connections,
            auto_migrate: cfg.db.auto_migrate,
        }
    }
}

/// Names of the auth backends an operator can enable.
///
/// The string form (`"local"`, `"oidc"`, `"passkey"`) is what shows
/// up in `[auth].backends` and `NAGENT_AUTH_BACKENDS`. The enum is
/// defined in `crate::config::AuthBackendKind` so the config code
/// stays free of feature gates.
pub use crate::config::AuthBackendKind;
