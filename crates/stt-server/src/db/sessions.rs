//! Sessions repository — **stub**.
//!
//! Plan 5.D will extract the session-row SQL from
//! [`crate::auth::db_sqlite`] and [`crate::auth::db_postgres`] into
//! here. For now the method bodies remain on [`crate::auth::store::AuthStore`]
//! so this module is intentionally empty — the per-domain split
//! lands in the next refactor commit.

use crate::db::pool::AnyPool;

/// Handle for the future sessions repository. Carries no state
/// today; the SQLite / Postgres pool lives in
/// [`crate::auth::store::AuthStore`] until the extraction commit.
#[derive(Debug, Clone)]
pub struct Sessions;

impl Sessions {
    /// Construct a stub repository bound to `pool`. Reserved for
    /// the extraction commit — every method on this stub will
    /// panic if called.
    pub fn new(_pool: &AnyPool) -> Self {
        Self
    }
}
