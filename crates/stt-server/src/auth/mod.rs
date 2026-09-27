//! `auth/` — multi-user authentication & user identity (PR1).
//!
//! The subsystem is gated behind the `auth` cargo feature. When the
//! feature is off this entire module compiles to a stub so the rest
//! of the crate does not need `#[cfg(feature = "auth")]` guards at
//! every call site.
//!
//! ## Sub-modules
//!
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
//! - [`mod@rate_limit`] — login-attempt rate-limit
//!   (DashMap, `(email, ip)` key).
//! - [`mod@cli`] — `stt-server auth {create-admin,list-users,delete-user}`.
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

#[cfg(feature = "auth")]
pub mod cli;
#[cfg(feature = "auth")]
pub mod db_postgres;
#[cfg(feature = "auth")]
pub mod db_sqlite;
#[cfg(feature = "auth")]
pub mod error;
#[cfg(feature = "auth")]
pub mod middleware;
#[cfg(feature = "auth")]
pub mod oidc;
#[cfg(feature = "auth")]
pub mod passkey;
#[cfg(feature = "auth")]
pub mod password;
#[cfg(feature = "auth")]
pub mod rate_limit;
#[cfg(feature = "auth")]
pub mod routes;
#[cfg(feature = "auth")]
pub mod session;
#[cfg(feature = "auth")]
pub mod store;

#[cfg(feature = "auth")]
pub use error::AuthError;
#[cfg(feature = "auth")]
pub use session::{AuthUser, SessionRecord};
#[cfg(feature = "auth")]
pub use store::AuthStore;

/// Names of the auth backends an operator can enable.
///
/// The string form (`"local"`, `"oidc"`, `"passkey"`) is what shows
/// up in `[auth].backends` and `NAGENT_AUTH_BACKENDS`. The enum is
/// defined in `crate::config::AuthBackendKind` so the config code
/// stays free of `#[cfg(feature = "auth")]` gates.
#[cfg(feature = "auth")]
pub use crate::config::AuthBackendKind;
