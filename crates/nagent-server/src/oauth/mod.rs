//! `oauth/` — PKCE client, state/nonce store, token refresh.
//!
//! Built so the OIDC backend (currently a stub — see
//! [`crate::auth::oidc`]) and the future X-timeline agent share the
//! same OAuth dance primitives:
//!
//! - [`pkce::PkcePair`] generates a verifier S256 challenge per
//! authorization request and verifies the round-trip on callback.
//! - [`state::StateStore`] holds the per-request `(state, nonce,
//! pkce_verifier, redirect_after, …)` blob between `/start` and
//! `/callback`. TTL is fixed at 10 minutes — well under the typical
//! 5-minute user interaction, well under the 1-hour cap most
//! identity providers enforce.
//! - [`refresh::RefreshTokenClient`] is a small trait the future
//! OIDC X integrations implement to mint a fresh access token
//! from a stored refresh token. leaves the wire calls unimplemented;
//! the trait is defined and unit-tested so  can drop the impl
//! in without touching the public surface.
//!
//! The module deliberately keeps no I/O beyond what `pkce` needs
//! (which is nothing) — the actual HTTP round-trips live in the
//! future OIDC backend and the X agent.

pub mod pkce;
pub mod refresh;
pub mod state;

#[cfg(feature = "x-agent")]
pub mod x;

pub use pkce::{PkcePair, PkceVerifier};
pub use refresh::{RefreshTokenClient, RefreshTokenError, TokenSet};
pub use state::{StateStore, StateStoreEntry};
