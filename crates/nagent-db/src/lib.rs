//! `nagent-db` — per-domain repositories over a shared sqlx pool.
//!
//! Plan 4.G splits the god `AuthStore` into per-domain modules,
//! each backed by a thin sqlite / postgres implementation against
//! the shared [`pool::AnyPool`]. The repositories depend only on
//! `nagent-db`; the domain crates (`auth`, `documents`, `chat`,
//! `credentials`) wire them together at the application root
//! (`nagent-server`).
//!
//! ## SQL policy
//!
//! SQL is per-engine today (sqlite vs postgres disagree on `?` vs
//! `$N` placeholders, `TEXT` vs `TIMESTAMP` types, and a few SQL
//! features). Each repository keeps its SQL next to the rust code
//! so the next refactor that consolidates dialects does not have
//! to crawl the call graph. We deliberately avoid `sqlx::Any`
//! until a concrete need pushes us there.

pub mod chat_sessions;
pub mod credentials;
pub mod documents;
pub mod error;
pub mod events;
pub mod migrate;
pub mod passkeys;
pub mod pool;
pub mod preferences;
pub mod sessions;
pub mod types;
pub mod users;

#[cfg(test)]
mod tests;

pub use pool::{AnyPool, DbEngine, DbOptions};

// Re-export the row types that the per-domain SQL
// serialises / deserialises so external callers can keep
// importing them from `nagent_db::types` regardless of which
// domain module they came from.
pub use error::Error;
pub use types::{
    AuthUser, AuthUserRecord, DocumentRow, MigrationRow, MigrationStatus, NewAuthEvent,
    NewPasskeyRecord, PasskeyRecord, SessionRecord, SessionSource, SessionTokenHash,
    UserCredentialRow, UserPreferences, SESSION_HASH_BYTES, SESSION_TOKEN_BYTES,
};
