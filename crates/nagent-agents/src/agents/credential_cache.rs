//! Per-request plaintext credential cache.
//!
//! Built as a `dashmap::DashMap<(service_id, field_key), SecretString>`
//! so the resolver can hand the same plaintext to multiple
//! `ctx.secret()` calls without hitting the DB / running AES-GCM
//! twice per round. Each `UserContext` owns exactly one
//! `SecretCache`; the `UserContext::drop` impl calls
//! [`SecretCache::zeroize`] so the plaintext only lives for the
//! lifetime of a single LLM tool call (or one direct
//! `/v1/agents/:name/invoke` round).
//!
//! There is no long-lived plaintext cache anywhere in the process —
//! the `SecretCache` is created inside `UserContext::new` and
//! destroyed when `UserContext` goes out of scope.

use dashmap::DashMap;
use secrecy::SecretString;

/// A small, per-request plaintext credential cache.
///
/// Thread-safe (`DashMap`) but intended to be owned by a single
/// `UserContext` and zeroised at the end of the request. Keyed by
/// `(service_id, field_key)` so a second lookup of the same field
/// inside the same request returns the cached `SecretString`
/// without touching the DB.
#[derive(Default, Debug)]
pub struct SecretCache {
    entries: DashMap<(String, String), SecretString>,
}

impl SecretCache {
    /// Empty cache; allocated inside [`crate::UserContext::new`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or overwrite) a plaintext value. The `SecretString` is
    /// stored wrapped so accidental `Debug` / log output cannot leak
    /// the bytes.
    pub fn insert(&self, service: &str, field: &str, value: SecretString) {
        self.entries
            .insert((service.to_string(), field.to_string()), value);
    }

    /// Look up a previously inserted plaintext. Returns a clone of
    /// the stored `SecretString` (cheap — the wrapper is just a
    /// `String` behind a guard).
    pub fn get(&self, service: &str, field: &str) -> Option<SecretString> {
        self.entries
            .get(&(service.to_string(), field.to_string()))
            .map(|kv| kv.value().clone())
    }

    /// Wipe every cached plaintext.
    ///
    /// Called from [`crate::UserContext::drop`] so the plaintext
    /// never outlives the request. The `SecretString` wrapper
    /// zeroises on drop (via the `zeroize` feature on the `secrecy`
    /// crate), so `DashMap::clear` is sufficient — every entry's
    /// value runs through its `Drop` impl, which wipes the backing
    /// `String` before releasing it.
    pub fn zeroize(&self) {
        self.entries.clear();
    }

    /// Number of cached entries; used by tests.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty; used by tests.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Drop for SecretCache {
    fn drop(&mut self) {
        self.zeroize();
    }
}
