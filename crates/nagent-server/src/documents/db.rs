//! `documents/db.rs` — re-export only.
//!
//! Plan 4.A: every per-user document query lives directly on
//! [`nagent_db::documents::Documents`] (scoped per user via
//! [`nagent_db::documents::Documents::for_user`]). Routes / CLI
//! drive it through `state.documents.db().documents...`.
//!
//! Kept as a re-export module so `super::db::DocumentError` keeps
//! resolving for the routes module + the CLI.

pub use nagent_db::documents::DocumentError;
