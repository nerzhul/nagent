//! Documents repository — **stub**.
//!
//! Plan 5.D will extract the `uploaded_documents` SQL from
//! [`crate::documents::db`] into here. The current implementation
//! lives in `documents/` until the extraction commit.

use crate::db::pool::AnyPool;

#[derive(Debug, Clone)]
pub struct Documents;

impl Documents {
    pub fn new(_pool: &AnyPool) -> Self {
        Self
    }
}
