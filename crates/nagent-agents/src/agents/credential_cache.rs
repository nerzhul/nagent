//! Per-request plaintext credential cache.
//!
//! Built on top of [`nagent_support::ttl_map::TtlMap`] so the map is
//! both bounded (the legacy `DashMap` could grow without limit
//! under a runaway LLM tool loop) and expiring (a stuck plaintext
//! disappears after the TTL instead of waiting for the
//! `UserContext` to drop). Each `UserContext` owns exactly one
//! [`SecretCache`]; `UserContext::drop` runs through
//! [`SecretCache::zeroize`] so the plaintext still never outlives
//! the request, but the TTL is the load-bearing mechanism
//! (plan 4.B / R2: every map reachable from low-privilege
//! requests must be bounded and expiring).
//!
//! There is no long-lived plaintext cache anywhere in the process
//! — the `SecretCache` is created inside `UserContext::new` and
//! destroyed when `UserContext` goes out of scope. The TTL is the
//! defence-in-depth backstop.

use std::time::Duration;

use nagent_support::ttl_map::TtlMap;
use secrecy::SecretString;

/// Default TTL for a cached credential plaintext. The per-request
/// `UserContext` is the authoritative scope, so this is the
/// ceiling: a stuck request that loses its `UserContext` (a future
/// caller error) gets at most this much time before the plaintext
/// vanishes.
pub const SECRET_CACHE_DEFAULT_TTL: Duration = Duration::from_secs(60);

/// Hard cap on the number of cached credentials per request.
/// A single tool loop rarely touches more than ~4 distinct
/// `(service, field)` pairs, so 16 leaves ample headroom and
/// still bounds memory under a misbehaving LLM.
pub const SECRET_CACHE_MAX_ENTRIES: usize = 16;

/// A small, per-request plaintext credential cache.
///
/// Thread-safe (`TtlMap` is `DashMap`-backed) but intended to be
/// owned by a single `UserContext` and zeroised at the end of the
/// request. Keyed by `(service_id, field_key)` so a second
/// lookup of the same field inside the same request returns the
/// cached `SecretString` without touching the DB.
pub struct SecretCache {
    entries: TtlMap<(String, String), SecretString>,
}

impl Default for SecretCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretCache {
    /// Empty cache with the default TTL + cap. Allocated inside
    /// [`crate::UserContext::new`].
    pub fn new() -> Self {
        Self::with_limits(SECRET_CACHE_DEFAULT_TTL, SECRET_CACHE_MAX_ENTRIES)
    }

    /// Build a cache with a custom TTL / cap so tests can verify
    /// the expiry and eviction paths without waiting 60 s.
    pub fn with_limits(ttl: Duration, max_entries: usize) -> Self {
        Self {
            entries: TtlMap::new(ttl, max_entries),
        }
    }

    /// Insert (or overwrite) a plaintext value. The
    /// `SecretString` is stored wrapped so accidental `Debug` /
    /// log output cannot leak the bytes. An insert restarts the
    /// entry's TTL clock.
    pub fn insert(&self, service: &str, field: &str, value: SecretString) {
        self.entries
            .insert((service.to_string(), field.to_string()), value);
    }

    /// Look up a previously inserted plaintext. Returns a clone
    /// of the stored `SecretString` (cheap — the wrapper is just a
    /// `String` behind a guard). [`TtlMap::get`] already returns
    /// an owned `V`, so no extra `.map(|v| v.clone())` is needed.
    pub fn get(&self, service: &str, field: &str) -> Option<SecretString> {
        self.entries
            .get(&(service.to_string(), field.to_string()))
    }

    /// Wipe every cached plaintext. Called from
    /// [`crate::UserContext::drop`] so the plaintext never
    /// outlives the request. The `TtlMap::clear` runs every
    /// entry's value through its `Drop` impl, which zeroises the
    /// backing `String` via the `secrecy` crate's `zeroize`
    /// feature.
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

impl std::fmt::Debug for SecretCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretCache")
            .field("len", &self.entries.len())
            .finish()
    }
}

impl Drop for SecretCache {
    fn drop(&mut self) {
        self.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    fn make_secret(s: &str) -> SecretString {
        secrecy::SecretString::new(String::from(s).into_boxed_str())
    }

    #[test]
    fn insert_and_get() {
        let cache = SecretCache::new();
        cache.insert("svc", "field", make_secret("hello"));
        assert_eq!(cache.get("svc", "field").unwrap().expose_secret(), "hello");
        assert!(cache.get("svc", "other").is_none());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn zeroize_clears_entries() {
        let cache = SecretCache::new();
        cache.insert("svc", "field", make_secret("hello"));
        assert_eq!(cache.len(), 1);
        cache.zeroize();
        assert!(cache.is_empty());
    }

    #[test]
    fn ttl_expires_entries() {
        // 50 ms is short enough for tests yet long enough to
        // not race the scheduler.
        let cache = SecretCache::with_limits(Duration::from_millis(50), 16);
        cache.insert("svc", "field", make_secret("hello"));
        assert_eq!(cache.len(), 1);
        std::thread::sleep(Duration::from_millis(120));
        // The next lookup must trigger the lazy expiry: the
        // value is gone.
        assert!(cache.get("svc", "field").is_none());
    }

    #[test]
    fn cap_drops_oldest_on_overflow() {
        // Tiny cap so we can test the eviction path without
        // stuffing the cache. 4 entries, 5th insert should drop
        // the oldest live one.
        let cache = SecretCache::with_limits(Duration::from_secs(60), 4);
        for i in 0..4 {
            cache.insert("svc", &format!("f{i}"), make_secret(&format!("v{i}")));
        }
        assert_eq!(cache.len(), 4);
        cache.insert("svc", "f4", make_secret("v4"));
        // The map is at its cap; the post-insert loop drops the
        // oldest. f0 was the first insert; f4 lands.
        assert_eq!(cache.len(), 4);
        assert!(
            cache.get("svc", "f0").is_none(),
            "oldest entry must be evicted"
        );
        assert!(
            cache.get("svc", "f4").is_some(),
            "newest entry must survive"
        );
    }
}
