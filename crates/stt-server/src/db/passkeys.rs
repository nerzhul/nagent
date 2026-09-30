//! Passkeys repository — **stub**.
//!
//! Plan 5.D will extract the passkey SQL from
//! [`crate::auth::db_sqlite`] / [`crate::auth::db_postgres`] into
//! here. The current methods remain on
//! [`crate::auth::store::AuthStore`] until that commit.

use crate::db::pool::AnyPool;

#[derive(Debug, Clone)]
pub struct Passkeys;

impl Passkeys {
    pub fn new(_pool: &AnyPool) -> Self {
        Self
    }
}
