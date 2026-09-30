//! Auth events repository — **stub**.
//!
//! Plan 5.D will extract the `auth_events` SQL from the auth
//! store. The current implementation lives on
//! [`crate::auth::store::AuthStore::record_event`] until the
//! extraction commit.

use crate::db::pool::AnyPool;

#[derive(Debug, Clone)]
pub struct Events;

impl Events {
    pub fn new(_pool: &AnyPool) -> Self {
        Self
    }
}
