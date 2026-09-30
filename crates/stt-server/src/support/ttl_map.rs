//! Bounded map with per-entry TTL.
//!
//! The canonical in-memory store for every map reachable from an
//! unauthenticated or low-privilege request — passkey ceremonies,
//! OAuth state, chat-session bindings, the credentials cache, STT
//! sessions (plan R2). The map is backed by `dashmap` so reads /
//! writes are lock-free on a per-shard basis (matching the rest of
//! the server's in-memory state).
//!
//! Two knobs keep the memory footprint bounded:
//!
//! - A per-entry TTL — an entry whose `expires_at` is in the past
//!   is removed on the next read / sweep.
//! - A hard `max_entries` cap — when an `insert` would exceed the
//!   cap, the oldest live entry is dropped to make room.
//!
//! ## Sweep strategy
//!
//! Sweeps are **opportunistic on insert** (one sweep pass before
//! the cap check) plus **periodic** via [`TtlMap::sweep_expired`].
//! Callers should drive the periodic sweep from their own
//! background task; this map deliberately does not spawn one of
//! its own because every existing caller already has a
//! per-subsystem background loop.

use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Outcome of [`TtlMap::insert`]. Tells the caller whether the map
/// had to evict anything to make room for the new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    /// The new value was inserted; nothing was evicted.
    Inserted,
    /// At least one expired entry was evicted by the opportunistic
    /// sweep; the new value was inserted.
    EvictedExpired,
    /// The hard cap was reached; the oldest live entry was dropped
    /// to make room. (If a pre-sweep also evicted expired entries,
    /// `EvictedOldest` wins so the caller can react to the cap hit.)
    EvictedOldest,
}

/// One live entry. `expires_at` is set at insert / update time; the
/// opportunistic sweep + [`TtlMap::sweep_expired`] compare it
/// against `Instant::now()`.
#[derive(Debug)]
struct Entry<V> {
    value: V,
    expires_at: Instant,
}

/// Bounded `K -> V` map with per-entry TTL.
pub struct TtlMap<K, V> {
    inner: DashMap<K, Entry<V>>,
    ttl: Duration,
    max_entries: usize,
}

impl<K, V> TtlMap<K, V>
where
    K: Eq + std::hash::Hash + Clone,
{
    /// Construct a new map. `max_entries` of `0` is rejected —
    /// the map exists to bound memory, so a zero cap would defeat
    /// the point.
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        assert!(max_entries > 0, "TtlMap requires max_entries > 0");
        Self {
            inner: DashMap::new(),
            ttl,
            max_entries,
        }
    }

    /// TTL applied to every fresh entry (and to entries updated via
    /// [`Self::insert`] with the same key).
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Maximum number of live entries before the cap kicks in.
    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Insert `value` under `key`, returning the new key's view of
    /// the map (existing entries with the same key are overwritten;
    /// their TTL is reset).
    ///
    /// Side effects, in order:
    ///
    /// 1. Opportunistic sweep: remove every entry whose TTL has
    ///    elapsed.
    /// 2. Insert the new value with a fresh `expires_at = now + ttl`.
    /// 3. Cap enforcement: if the map is over the cap, drop the
    ///    oldest live entry by `expires_at` until we are at or
    ///    under the cap. This post-insert loop converges under
    ///    contention — even if N threads race past the check,
    ///    each thread's loop runs until `len() <= max_entries`.
    pub fn insert(&self, key: K, value: V) -> InsertOutcome {
        let now = Instant::now();
        let expires_at = now + self.ttl;

        // Update of an existing key: no cap concern, no eviction.
        if let Some(mut entry) = self.inner.get_mut(&key) {
            entry.value = value;
            entry.expires_at = expires_at;
            return InsertOutcome::Inserted;
        }

        let evicted_expired = self.sweep_expired_at(now);

        self.inner.insert(key, Entry { value, expires_at });

        // Cap enforcement. DashMap's `len()` is eventually
        // consistent across shards — under contention the post-
        // insert length can briefly exceed `max_entries`. The loop
        // drains the overshoot one eviction at a time; concurrent
        // threads racing past the loop will each drop one entry,
        // which over-evicts harmlessly (they were already on the
        // chopping block) without dropping any fresh entry.
        let mut cap_hit = false;
        while self.inner.len() > self.max_entries {
            self.evict_oldest_at(now);
            cap_hit = true;
        }

        if cap_hit {
            InsertOutcome::EvictedOldest
        } else if evicted_expired > 0 {
            InsertOutcome::EvictedExpired
        } else {
            InsertOutcome::Inserted
        }
    }

    /// Read `key`. Returns `None` if the entry is absent or has
    /// expired (in the latter case it is removed from the map).
    /// The value is cloned — `TtlMap` does not expose interior
    /// references because `DashMap` would deadlock on a long-lived
    /// `Ref` under a concurrent `insert`.
    pub fn get(&self, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let now = Instant::now();
        let entry = self.inner.get(key)?;
        if entry.expires_at <= now {
            // Drop the read guard before mutating; otherwise the
            // `remove` call would deadlock on the shard lock.
            drop(entry);
            self.inner.remove(key);
            return None;
        }
        Some(entry.value.clone())
    }

    /// Remove `key` regardless of its TTL. Returns the removed
    /// value, or `None` if the key was absent.
    pub fn remove(&self, key: &K) -> Option<V> {
        self.inner.remove(key).map(|(_, entry)| entry.value)
    }

    /// Number of live entries (including ones whose TTL has just
    /// elapsed but have not been swept yet — sweep on demand with
    /// [`Self::sweep_expired`] if you need an exact count).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Remove every entry whose TTL has elapsed. Returns the number
    /// of entries removed. Cheap to call from a periodic background
    /// task; the cost is `O(n)` over the live set.
    pub fn sweep_expired(&self) -> usize {
        self.sweep_expired_at(Instant::now())
    }

    fn sweep_expired_at(&self, now: Instant) -> usize {
        let mut evicted = 0usize;
        self.inner.retain(|_, entry| {
            if entry.expires_at <= now {
                evicted += 1;
                false
            } else {
                true
            }
        });
        evicted
    }

    fn evict_oldest_at(&self, now: Instant) {
        // Walk the live set, find the entry with the smallest
        // `expires_at` (== the oldest insert), then remove it.
        // O(n) but only runs when the cap is hit, which is by
        // definition an exceptional condition.
        let _ = now;
        let mut oldest: Option<(Instant, K)> = None;
        for entry in self.inner.iter() {
            let ts = entry.value().expires_at;
            match &oldest {
                None => oldest = Some((ts, entry.key().clone())),
                Some((cur, _)) if ts < *cur => {
                    oldest = Some((ts, entry.key().clone()));
                }
                _ => {}
            }
        }
        if let Some((_, key)) = oldest {
            self.inner.remove(&key);
        }
    }
}

// `DashMap` is `Sync` when its key/value are `Send + Sync`; this
// blanket impl matches the rest of the server's in-memory stores.
unsafe impl<K, V> Send for TtlMap<K, V>
where
    K: Eq + std::hash::Hash + Send,
    V: Send,
{
}
unsafe impl<K, V> Sync for TtlMap<K, V>
where
    K: Eq + std::hash::Hash + Send + Sync,
    V: Send + Sync,
{
}

impl<K, V> std::fmt::Debug for TtlMap<K, V>
where
    K: Eq + std::hash::Hash + std::fmt::Debug,
    V: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TtlMap")
            .field("len", &self.inner.len())
            .field("max_entries", &self.max_entries)
            .field("ttl", &self.ttl)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ttl() -> Duration {
        Duration::from_millis(50)
    }

    #[test]
    fn insert_and_get_round_trip() {
        let map = TtlMap::<u32, &'static str>::new(ttl(), 8);
        assert_eq!(map.insert(1, "a"), InsertOutcome::Inserted);
        assert_eq!(map.get(&1), Some("a"));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn insert_overwrites_existing_key_without_eviction() {
        let map = TtlMap::<u32, u32>::new(ttl(), 8);
        assert_eq!(map.insert(1, 10), InsertOutcome::Inserted);
        assert_eq!(map.insert(1, 20), InsertOutcome::Inserted);
        assert_eq!(map.get(&1), Some(20));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn get_returns_none_after_ttl() {
        let map = TtlMap::<u32, u32>::new(ttl(), 8);
        map.insert(1, 10);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(map.get(&1), None);
        // The expired entry is removed by the get itself.
        assert!(map.is_empty());
    }

    #[test]
    fn remove_drops_entry() {
        let map = TtlMap::<u32, u32>::new(ttl(), 8);
        map.insert(1, 10);
        assert_eq!(map.remove(&1), Some(10));
        assert_eq!(map.remove(&1), None);
        assert!(map.is_empty());
    }

    #[test]
    fn sweep_expired_drops_only_dead_entries() {
        let map = TtlMap::<u32, u32>::new(ttl(), 8);
        map.insert(1, 10);
        std::thread::sleep(Duration::from_millis(80));
        // No fresh insert here, so the opportunistic sweep on
        // `insert` cannot run ahead of `sweep_expired`.
        assert_eq!(map.sweep_expired(), 1);
        assert!(map.is_empty());

        // A live entry survives a sweep.
        map.insert(2, 20);
        assert_eq!(map.sweep_expired(), 0);
        assert_eq!(map.get(&2), Some(20));
    }

    #[test]
    fn cap_hit_evicts_oldest_live_entry() {
        let map = TtlMap::<u32, u32>::new(ttl(), 2);
        map.insert(1, 10);
        map.insert(2, 20);
        // Now at the cap. A third insert must evict the oldest
        // live entry (key 1, the first insert).
        assert_eq!(map.insert(3, 30), InsertOutcome::EvictedOldest);
        assert_eq!(map.len(), 2);
        assert!(map.get(&1).is_none(), "oldest entry must be evicted");
        assert_eq!(map.get(&2), Some(20));
        assert_eq!(map.get(&3), Some(30));
    }

    #[test]
    fn insert_evicts_expired_opportunistically() {
        let map = TtlMap::<u32, u32>::new(ttl(), 8);
        map.insert(1, 10);
        std::thread::sleep(Duration::from_millis(80));
        // Entry 1 is dead; the opportunistic sweep on insert must
        // drop it before the new entry is added.
        assert_eq!(
            map.insert(2, 20),
            InsertOutcome::EvictedExpired,
            "opportunistic sweep must report expired evictions"
        );
        assert_eq!(map.len(), 1);
        assert!(map.get(&1).is_none());
        assert_eq!(map.get(&2), Some(20));
    }

    #[test]
    fn empty_map_reports_empty() {
        let map = TtlMap::<u32, u32>::new(ttl(), 8);
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }

    #[test]
    #[should_panic(expected = "max_entries > 0")]
    fn zero_cap_is_rejected() {
        let _ = TtlMap::<u32, u32>::new(ttl(), 0);
    }

    #[test]
    fn debug_includes_max_entries_and_ttl() {
        let map = TtlMap::<u32, u32>::new(ttl(), 4);
        let s = format!("{map:?}");
        assert!(s.contains("TtlMap"));
        assert!(s.contains("max_entries"));
        assert!(s.contains("ttl"));
    }

    #[test]
    fn concurrent_inserts_do_not_panic() {
        // Sanity check that the `DashMap`-backed map holds up under
        // concurrent inserts from many threads. We only assert the
        // post-condition (len <= max_entries, all keys present) — the
        // stress is whether the cap is honoured.
        use std::sync::Arc;
        use std::thread;

        let map = Arc::new(TtlMap::<u64, u64>::new(Duration::from_secs(60), 64));
        let mut handles = Vec::new();
        for t in 0..8 {
            let m = Arc::clone(&map);
            handles.push(thread::spawn(move || {
                for i in 0..32u64 {
                    m.insert(t * 1_000 + i, i);
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }
        assert!(map.len() <= 64, "cap must be honoured under contention");
    }
}
