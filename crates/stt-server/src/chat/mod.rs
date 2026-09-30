//! `chat/` — server-bound chat-session bookkeeping.
//!
//! The mint/bind handlers live in [`crate::documents::routes`] for now
//! (they share the documents router and CORS envelope); this module
//! owns the in-memory [`sessions::ChatSessions`] handle that backs the
//! `(user, session)` SEV 2 binding.

pub mod sessions;
