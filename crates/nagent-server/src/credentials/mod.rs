//! Per-user credentials framework.
//!
//! Lets the chat agent subsystem store, retrieve, and audit
//! hostname/login/password tuples per (user, service, field). Plaintext
//! values are never logged or persisted in clear; ciphertext lives in
//! `user_credentials`, encryption happens through [`aes_gcm`] with a
//! server-side key loaded from `[auth.credentials].key` at boot.
//!
//! Module layout:
//! - [`key`]: `CredentialsKey` — zeroized `Secret<[u8; 32]>` parsed from
//! a hex env var.
//! - [`crypto`]: `seal`/`open` thin wrappers around `aes-gcm`.
//! - [`resolver`]: `CredentialResolver` — async reader that decrypts
//! on demand and writes one audit row per call. The per-request
//! plaintext cache used to live here but moved to
//! `nagent_agents::credential_cache` (plan 4.C.2 / R2) so it can
//! share the `TtlMap` infrastructure every other per-user map
//! uses.
//! - [`routes`]: HTTP handlers for the `/api/integrations*` family.
//!
//! The trait evolution lives in [`crate::agents`]: every agent takes
//! `&UserContext` as its first argument and resolves secrets via
//! `ctx.secret("svc", "field")`.

pub mod crypto;
pub mod key;
pub mod resolver;
pub mod routes;

#[cfg(feature = "caldav-agent")]
pub mod caldav_probe;

pub use crypto::{decrypt, encrypt, EncryptedSecret};
pub use key::{CredentialsKey, CredentialsKeyError};
pub use resolver::{CredentialError, CredentialResolver};
