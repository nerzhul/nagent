//! `db/` — per-domain repositories over a shared pool.
//!
//! Plan 5.D splits the god [`crate::auth::store::AuthStore`] (and
//! the auxiliary tables in `documents` / `chat`) into one module per
//! domain (`users`, `sessions`, `passkeys`, `events`, `credentials`,
//! `preferences`, `documents`, `chat_sessions`), each backed by a
//! thin sqlite / postgres implementation against a shared pool /
//! migration runner. The repositories depend only on `db`; the
//! domain crates (`auth`, `documents`, `chat`, `credentials`) wire
//! them together at the application root.
//!
//! ## Layout
//!
//! - [`pool`] — `AnyPool` (the engine-agnostic handle) plus the
//!   `connect_from_auth_config` builder that the boot path uses to
//!   materialise a pool from `config::AuthConfig`.
//! - [`migrate`] — wraps `sqlx::migrate!` with status / revert
//!   helpers shared by both engines. The migration files now live
//!   under [`migrations`].
//! - [`users`], [`sessions`], [`passkeys`], [`events`],
//!   [`credentials`], [`preferences`], [`documents`],
//!   [`chat_sessions`] — one module per domain. Each module owns the
//!   row types and the per-engine SQL.
//!
//! ## SQL policy
//!
//! SQL is per-engine today (sqlite vs postgres disagree on `?` vs
//! `$N` placeholders, `TEXT` vs `TIMESTAMP` types, and a few SQL
//! features). Each repository keeps its SQL next to the rust code so
//! the next refactor that consolidates dialects does not have to
//! crawl the call graph. We deliberately avoid `sqlx::Any` until a
//! concrete need pushes us there.

pub mod chat_sessions;
pub mod credentials;
pub mod documents;
pub mod events;
pub mod migrate;
pub mod passkeys;
pub mod pool;
pub mod preferences;
pub mod sessions;
pub mod users;

#[cfg(test)]
mod tests;

pub use pool::{connect_from_auth_config, AnyPool, DbEngine};
