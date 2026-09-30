//! `chat/` — server-bound chat-session bookkeeping.
//!
//! The `ChatSessions` handle lives in [`crate::chat::sessions`]. It
//! binds a `(user, session)` pair so the SEV 2 audit fix can verify
//! a `X-Chat-Session-Id` request header against the binding before
//! the `read_document` agent or the documents routes touch the DB.
//!
//! The mint/bind handlers live in [`crate::documents::routes`] for
//! now (they share the documents router and CORS envelope); this
//! module owns the in-memory [`sessions::ChatSessions`] handle that
//! backs the binding.

pub mod sessions;
