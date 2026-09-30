//! Chat sessions repository — **stub**.
//!
//! Plan 5.D will extract the `chat_sessions` SQL from
//! [`crate::chat::sessions`] into here. The current implementation
//! lives in `chat/` until the extraction commit.

use crate::db::pool::AnyPool;

#[derive(Debug, Clone)]
pub struct ChatSessions;

impl ChatSessions {
    pub fn new(_pool: &AnyPool) -> Self {
        Self
    }
}
