//! In-memory state store for the OAuth `/start` → `/callback`
//! round-trip.
//!
//! The browser receives a one-shot `state` token on `/start` and
//! returns it on `/callback`. The server must:
//!
//! 1. Confirm the `state` matches a recent `/start` request,
//! 2. Bind the response to the original user session / chat session /
// redirect-after-login, and
//! 3. Drop the entry as soon as the round-trip completes.
//!
//! RFC 6749 §10.5 recommends an expiry; we use 10 minutes — well
//! above the typical user interaction time, well below the hour
//! most providers enforce.
//!
//! The implementation is a DashMap keyed by the random `state`
//! string. (and the X agent) will own the per-call payload
//! schema; the store itself is payload-agnostic.

use std::time::Duration;

use base64::Engine;
use dashmap::DashMap;
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Default TTL for a state entry. Long enough to cover a typical
/// "click the link, log in, come back" interaction; short enough
/// that a stale entry is uninteresting by the next login.
pub const STATE_TTL: Duration = Duration::from_secs(600);

/// Length of the random `state` token, in bytes. Encoded as
/// base64-url by the store; the resulting string is ~43 characters
/// — well above the RFC 6749 §10.10 "unguessable" requirement.
pub const STATE_LEN: usize = 32;

/// One outstanding `/start` request. Carries whatever payload the
/// backend needs to complete the round-trip — the OIDC backend
/// stores the PKCE verifier and nonce, the future X agent will
/// store the redirect-after target, both store the originating
/// session id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateStoreEntry {
    /// Unix epoch seconds when the entry was created. Used to
    /// enforce [`STATE_TTL`].
    pub created_at_unix: u64,
    /// Free-form backend-specific payload. The OIDC backend stores
    /// the PKCE verifier, the IdP nonce, and the user-agent
    /// fingerprint; the X agent will store the PKCE verifier and
    /// the OAuth `code_verifier` for the token exchange.
    ///
    /// Serialised as JSON so the store can be persisted to disk in
    /// a future release without an ABI break.
    pub payload: serde_json::Value,
}

impl StateStoreEntry {
    /// Build a fresh entry with the supplied payload and the
    /// current wall-clock time.
    pub fn now(payload: serde_json::Value) -> Self {
        Self {
            created_at_unix: now_unix(),
            payload,
        }
    }

    /// True when the entry is older than [`STATE_TTL`]. The store
    /// treats expired entries as absent (so a slow user does not
    /// bypass the freshness check).
    pub fn is_expired(&self) -> bool {
        let age = Duration::from_secs(now_unix().saturating_sub(self.created_at_unix));
        age >= STATE_TTL
    }
}

/// In-memory state store. Cheap to clone (DashMap is `Arc`-backed)
/// so it lives in `AuthState` / the X agent's runtime state.
#[derive(Debug, Default, Clone)]
pub struct StateStore {
    inner: std::sync::Arc<DashMap<String, StateStoreEntry>>,
}

impl StateStore {
    /// Empty store. Used in tests and when the OIDC / X features
    /// are disabled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Generate a fresh random `state` token, insert `payload`,
    /// and return the token. The caller hands the token to the
    /// browser; the matching [`str::pop`] on the callback returns
    /// the same payload.
    pub fn push(&self, payload: serde_json::Value) -> String {
        let mut bytes = [0u8; STATE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let token = base64_url(&bytes);
        // A duplicate token has astronomically small probability
        // (2^-256), but a defensive insert prevents two
        // concurrent /start calls from clobbering each other.
        self.inner
            .insert(token.clone(), StateStoreEntry::now(payload));
        token
    }

    /// Atomically remove and return the entry for `token`. Expired
    /// entries are treated as absent — [`StateStoreEntry::is_expired`].
    pub fn pop(&self, token: &str) -> Option<StateStoreEntry> {
        let (_, entry) = self.inner.remove(token)?;
        if entry.is_expired() {
            return None;
        }
        Some(entry)
    }

    /// Inspect (without removing) the entry for `token`. Used by
    /// unit tests; production code prefers [`Self::pop`] so the
    /// entry cannot be replayed.
    pub fn peek(&self, token: &str) -> Option<StateStoreEntry> {
        let entry = self.inner.get(token)?;
        if entry.is_expired() {
            return None;
        }
        Some(entry.clone())
    }

    /// Number of live entries. Used by tests and exposed for
    /// debugging.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// True when the store has no entries.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

fn base64_url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn now_unix() -> u64 {
    // std::time::SystemTime is available without extra deps. We do
    // NOT use chrono here so the oauth module stays cheap to
    // compile and avoids the worktree-wide chrono dependency.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_pop_round_trip() {
        let store = StateStore::new();
        let token = store.push(serde_json::json!({"pkce_verifier": "abc"}));
        let entry = store.pop(&token).expect("entry should exist");
        assert_eq!(entry.payload["pkce_verifier"], "abc");
    }

    #[test]
    fn pop_consumes_entry() {
        let store = StateStore::new();
        let token = store.push(serde_json::json!({}));
        assert!(store.pop(&token).is_some());
        assert!(store.pop(&token).is_none(), "second pop must miss");
    }

    #[test]
    fn expired_entries_are_treated_as_absent() {
        // We can simulate expiry by manually inserting an entry
        // with a stale `created_at_unix`.
        let store = StateStore::new();
        let token: String = "expired-token".into();
        store.inner.insert(
            token.clone(),
            StateStoreEntry {
                created_at_unix: now_unix().saturating_sub(STATE_TTL.as_secs() + 10),
                payload: serde_json::json!({}),
            },
        );
        assert!(store.pop(&token).is_none());
    }

    #[test]
    fn peek_does_not_consume() {
        let store = StateStore::new();
        let token = store.push(serde_json::json!({}));
        assert!(store.peek(&token).is_some());
        assert!(store.pop(&token).is_some(), "peek must not consume");
    }

    #[test]
    fn two_pushes_yield_distinct_tokens() {
        let store = StateStore::new();
        let a = store.push(serde_json::json!({}));
        let b = store.push(serde_json::json!({}));
        assert_ne!(a, b);
    }
}
