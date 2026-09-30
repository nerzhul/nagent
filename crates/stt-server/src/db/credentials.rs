//! Per-user credentials repository — **stub**.
//!
//! Plan 5.D will extract the `user_credentials` SQL from the auth
//! store. The current methods live on
//! [`crate::auth::store::AuthStore`] until the extraction commit.

use crate::db::pool::AnyPool;

#[derive(Debug, Clone)]
pub struct Credentials;

impl Credentials {
    pub fn new(_pool: &AnyPool) -> Self {
        Self
    }
}
