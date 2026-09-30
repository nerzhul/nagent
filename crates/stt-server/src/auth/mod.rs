//! `auth/` — multi-user authentication & user identity (PR1).
//!
//! Always compiled in (no cargo feature gate). When the runtime
//! config sets `auth.enabled = false` the server keeps the pre-PR1
//! single-user trust boundary; the modules in this tree still exist
//! but the `auth::boot::auto_bootstrap` function short-circuits and
//! the HTTP routes (registered in `http::build_router`) are guarded
//! by the runtime flag.
//!
//! ## Sub-modules
//!
//! - [`mod@boot`] — auto-bootstrap invoked by `main.rs` (migrate +
//!   optional sqlite first-admin creation).
//! - [`mod@store`] — DB-agnostic data access (users + sessions +
//!   passkeys + auth events). The public type is [`store::AuthStore`]
//!   which is a `match`-dispatched enum over the sqlite / postgres
//!   implementations (see [`db_sqlite`] / [`db_postgres`]).
//! - [`mod@session`] — cookie + bearer parsing, session row type,
//!   CSRF token generator, `Set-Cookie` builder.
//! - [`mod@middleware`] — `require_auth_middleware` axum middleware
//!   that resolves a session into an [`AuthUser`] extension.
//! - [`mod@password`] — argon2id wrapper + the `local` backend
//!   (`POST /api/auth/login/password`).
//! - [`mod@oidc`] — OIDC backend (`/api/auth/login/oidc/{start,callback}`).
//! - [`mod@passkey`] — WebAuthn backend (`/api/auth/login/passkey/{start,finish}`).
//! - [`mod@routes`] — `/api/me`, `/api/auth/logout`, plus the
//!   registration endpoints guarded by `RequireAuth`.
//! - [`mod@login_rate_limit`] — login-attempt rate-limit
//!   (DashMap, `(email, ip)` key).
//!
//! The `stt-server auth {create-admin,list-users,delete-user}` CLI
//! subcommand lives in [`crate::cli::auth`].
//!
//! ## Topology
//!
//! ```text
//! HTTP request ──► require_auth_middleware ──► AuthUser extension
//!                       │
//!                       └─► session.rs (cookie/bearer/CSRF)
//!                              └─► store.rs (UserStore/SessionStore)
//!                                     └─► db_sqlite / db_postgres
//! ```

pub mod boot;
pub mod db_postgres;
pub mod db_sqlite;
pub mod error;
pub mod login_rate_limit;
pub mod middleware;
pub mod oidc;
pub mod passkey;
pub mod password;
pub mod router;
pub mod routes;
pub mod session;
pub mod store;

pub use crate::state::AuthState;
pub use error::AuthError;
pub use error::{require_auth_store, require_oidc_state, require_passkey_state};
pub use oidc::OidcState;
pub use passkey::PasskeyState;
pub use session::{AuthUser, SessionRecord};
pub use store::AuthStore;
pub use store::UserPreferences;

/// Names of the auth backends an operator can enable.
///
/// The string form (`"local"`, `"oidc"`, `"passkey"`) is what shows
/// up in `[auth].backends` and `NAGENT_AUTH_BACKENDS`. The enum is
/// defined in `crate::config::AuthBackendKind` so the config code
/// stays free of feature gates.
pub use crate::config::AuthBackendKind;
