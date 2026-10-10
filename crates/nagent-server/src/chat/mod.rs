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
//!
//! [`messages::ChatMessages`] is the A3 (edit / regenerate) +
//! A1 (export) backing store: the per-session message rows that
//! the browser keeps in localStorage are now mirrored server-side
//! in `chat_messages`. The HTTP routes mounted under
//! `/v1/chat/session/:sid/messages*` are defined in
//! [`crate::documents::routes::build_chat_session_router`] (the
//! same router that already carries `POST /v1/chat/session`).

pub mod messages;
pub mod sessions;
