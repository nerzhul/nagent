//! `nagent-db` error type.
//!
//! The repositories return `Result<T, Error>`. The server-side
//! [`crate::auth::error::Error`] (which lives in
//! `nagent-server` and handles the HTTP `IntoResponse` mapping)
//! is built on top of this enum via a `From` impl in the
//! server crate.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// Underlying database error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A TEXT-shaped column could not be parsed as a UUID. Should
    /// not happen in practice (the Rust side is the only writer)
    /// but surfaces a clean message instead of a panic when the
    /// schema drifts.
    #[error("invalid uuid in database column: {0}")]
    InvalidUuid(#[from] uuid::Error),
    /// The caller violated a server-side invariant (e.g. wrong
    /// nonce length).
    #[error("bad request: {0}")]
    BadRequest(String),
    /// The supplied value is already taken.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Internal misconfiguration.
    #[error("internal: {0}")]
    Internal(String),
}
