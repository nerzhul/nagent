//! Per-user preferences repository — **stub**.
//!
//! Plan 5.D will extract the `user_preferences` SQL from the auth
//! store. The current methods live on
//! [`crate::auth::store::AuthStore`] until the extraction commit.

use crate::db::pool::AnyPool;

#[derive(Debug, Clone)]
pub struct Preferences;

impl Preferences {
    pub fn new(_pool: &AnyPool) -> Self {
        Self
    }
}
