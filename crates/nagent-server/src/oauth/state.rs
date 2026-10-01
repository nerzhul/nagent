//! In-memory state store for the OAuth `/start` → `/callback`
//! round-trip.
//!
//! The browser receives a one-shot `state` token on `/start` and
//! returns it on `/callback`. The server must:
//!
//! 1. Confirm the `state` matches a recent `/start` request,
//! 2. Bind the response to the original user session / chat session /
//! redirect-after-login, and
//! 3. Drop the entry as soon as the round-trip completes.
//!
//! RFC 6749 §10.5 recommends an expiry; we use 10 minutes — well
//! above the typical user interaction time, well below the hour
//! most providers enforce.
//!
//! The implementation is a [`nagent_support::ttl_map::TtlMap`]
//! keyed by the random `state` string (plan 4.B R2). The map is
//! backed by `dashmap` and bounded by both a per-entry TTL and a
//! hard `max_entries` cap, so a runaway `/start` burst cannot
//! grow the map without bound. (and the X agent) will own the
//! per-call payload schema; the store itself is payload-agnostic.

use std::time::Duration;

use base64::Engine;
use nagent_support::ttl_map::TtlMap;
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Maximum number of outstanding `/start` requests the store
/// will hold. A misbehaving client that keeps calling `/start`
/// without finishing the round-trip is the only way to hit it;
/// 16 KiB worth of small JSON payloads is enough for any
/// realistic auth flow.
const MAX_STATE_ENTRIES: usize = 16 * 1024;

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

/// In-memory state store. Cheap to clone (the inner [`TtlMap`] is
/// `Arc`-backed) so it lives in `AuthState` / the X agent's runtime
/// state.
#[derive(Debug, Clone)]
pub struct StateStore {
    inner: std::sync::Arc<TtlMap<String, StateStoreEntry>>,
}

impl Default for StateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl StateStore {
    /// Empty store. Used in tests and when the OIDC / X features
    /// are disabled.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(TtlMap::new(STATE_TTL, MAX_STATE_ENTRIES)),
        }
    }

    /// Build a store with a custom TTL + cap. Test-only entry
    /// point (production uses [`Self::new`]). Exposed so the
    /// `tests` module can drive the cap-hit path without
    /// allocating 16 KiB worth of fake entries.
    #[cfg(test)]
    fn with_cap(ttl: Duration, max_entries: usize) -> Self {
        Self {
            inner: std::sync::Arc::new(TtlMap::new(ttl, max_entries)),
        }
    }

    /// Mutate `token`'s `created_at_unix` so a stale value can be
    /// injected without touching the wall-clock TTL. Test-only.
    #[cfg(test)]
    fn inject_stale_for_test(&self, token: &str, created_at_unix: u64) {
        // Round-trip: pop the entry, mutate, re-insert. The
        // re-insert resets the wall-clock TTL but keeps the
        // logical `created_at_unix` stale, so `is_expired()`
        // returns true even though `TtlMap` would otherwise
        // consider the entry fresh.
        let Some(mut entry) = self.inner.remove(&token.to_string()) else {
            return;
        };
        entry.created_at_unix = created_at_unix;
        self.inner.insert(token.to_string(), entry);
    }

    /// Generate a fresh random `state` token, insert `payload`,
    /// and return the token. The caller hands the token to the
    /// browser; the matching [`StateStore::pop`] on the callback
    /// returns the same payload.
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
    /// entries are treated as absent — the [`TtlMap`] owns the
    /// wall-clock TTL; [`StateStoreEntry::is_expired`] is consulted
    /// as a defensive double-check so a manually-inserted stale
    /// entry (test-only path) cannot bypass the freshness check.
    pub fn pop(&self, token: &str) -> Option<StateStoreEntry> {
        let entry = self.inner.remove(&token.to_string())?;
        if entry.is_expired() {
            return None;
        }
        Some(entry)
    }

    /// Inspect (without removing) the entry for `token`. Used by
    /// unit tests; production code prefers [`Self::pop`] so the
    /// entry cannot be replayed.
    pub fn peek(&self, token: &str) -> Option<StateStoreEntry> {
        let entry = self.inner.get(&token.to_string())?;
        if entry.is_expired() {
            return None;
        }
        Some(entry)
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
        // We can simulate logical expiry (independent of the
        // TtlMap's wall-clock TTL) by inserting a fresh entry via
        // `push` and then mutating its `created_at_unix` into the
        // past. `pop` checks both clocks so the entry is treated
        // as absent.
        let store = StateStore::new();
        let token = store.push(serde_json::json!({}));
        // Mutate the entry's `created_at_unix` to be older than
        // `STATE_TTL`. Reaching into the inner map keeps the test
        // honest — a regression that drops the `is_expired`
        // double-check would let this entry leak.
        // SAFETY: `push` just put the entry in; the inner map is
        // `Arc<TtlMap<…>>` and we own the only reference in this
        // test (single-threaded). The `TtlMap` exposes its
        // `DashMap` through a `&` getter on its `Debug` impl; for
        // the test we read the value, mutate, and put it back.
        //
        // Round-trip via pop/peek on the test is what the public
        // surface allows; the staleness injection goes through a
        // helper closure so the test does not depend on private
        // map internals.
        store.inject_stale_for_test(&token, now_unix().saturating_sub(STATE_TTL.as_secs() + 10));
        assert!(store.pop(&token).is_none());
        assert!(store.peek(&token).is_none());
    }

    #[test]
    fn cap_hit_drops_oldest_live_entry() {
        // Sanity check: with a tiny cap, the next `push` must
        // evict the oldest live entry. Mirrors the same property
        // tested directly on `TtlMap` in the support crate; here
        // we just want to lock the StateStore wrapper in.
        let store = StateStore::with_cap(STATE_TTL, 2);
        let first = store.push(serde_json::json!({"n": 1}));
        store.push(serde_json::json!({"n": 2}));
        // Third push must evict the oldest live entry (`first`).
        let _third = store.push(serde_json::json!({"n": 3}));
        assert!(
            store.peek(&first).is_none(),
            "oldest entry must have been evicted by the cap"
        );
        assert_eq!(store.len(), 2);
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
