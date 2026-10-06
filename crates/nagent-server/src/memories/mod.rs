//! Per-user long-term memory subsystem (plan 1791267136806).
//!
//! Two sub-modules:
//! - [`adapter`] — `UserDbMemorySource`, the server-side impl of
//!   `nagent_agents::agents::MemorySource` that owns the AES-256-GCM
//!   encryption key and threads it into the chat path.
//! - [`routes`] — `GET /api/memories` + `DELETE /api/memories/:id`,
//!   mounted on the auth-protected subtree.

pub mod adapter;
pub mod routes;
