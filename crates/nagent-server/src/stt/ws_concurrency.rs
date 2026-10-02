//! WebSocket concurrency caps (plan S-1).
//!
//! Two independent caps are enforced at the HTTP upgrade:
//!
//! - **Global cap** (`ws_max_concurrent`): total active sessions
//!   across the whole process. Defends the inference queue from a
//!   flood of parallel upgrades.
//! - **Per-source-IP cap** (`ws_max_per_ip`): one attacker opening
//!   dozens of parallel upgrades must not be able to saturate the
//!   global cap.
//!
//! Reached caps are reported as `503` + `Retry-After: 1` instead of
//! silently dropping the connection — the operator must see the
//! rejections in plain HTTP logs without decoding a WS close
//! frame. Both counters are incremented in
//! [`register_session`] / decremented in [`unregister_session`] so
//! the bookkeeping tracks the [`SessionMap`] lifecycle exactly
//! (one slot per active WS, even if the underlying TCP socket
//! has already closed).
//!
//! The per-IP counter is a `DashMap<IpAddr, AtomicUsize>`; idle
//! entries are evicted on every call to keep the key space
//! bounded (mirrors [`crate::rate_limit::RateLimiter`]'s sweep
//! strategy).

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use dashmap::DashMap;

/// Outcome of a [`WsConcurrency::try_admit`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum AdmitDecision {
    /// The upgrade is allowed and the slots have been reserved.
    Admitted,
    /// The global cap is reached. The caller must reject with
    /// `503` + `Retry-After`.
    GlobalFull,
    /// The per-source-IP cap is reached. The caller must reject
    /// with `503` + `Retry-After`.
    PerIpFull,
}

/// Global + per-IP WebSocket concurrency counter. Cheap to
/// clone (both fields are `Arc`-wrapped); lives in
/// [`crate::state::SttState`].
#[derive(Clone)]
pub struct WsConcurrency {
    inner: Arc<WsConcurrencyInner>,
}

struct WsConcurrencyInner {
    /// Configured global cap (`ws_max_concurrent`). Read once at
    /// boot, kept here so the counter has no dependency on the
    /// outer `Config`.
    global_cap: usize,
    /// Configured per-IP cap (`ws_max_per_ip`).
    per_ip_cap: usize,
    /// Current count of active WebSocket sessions.
    global_count: AtomicUsize,
    /// Per-IP active-session counters. The map key is the source
    /// IP as resolved through [`crate::rate_limit::resolve_client_ip`]
    /// (loopback bypass is applied by the caller — the counter
    /// itself is a dumb bucket).
    per_ip: DashMap<IpAddr, AtomicUsize>,
}

impl std::fmt::Debug for WsConcurrency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsConcurrency")
            .field("global_cap", &self.inner.global_cap)
            .field("per_ip_cap", &self.inner.per_ip_cap)
            .field("global_count", &self.inner.global_count)
            .field("per_ip_keys", &self.inner.per_ip.len())
            .finish()
    }
}

impl WsConcurrency {
    /// Build a new concurrency counter with the supplied caps.
    pub fn new(global_cap: usize, per_ip_cap: usize) -> Self {
        Self {
            inner: Arc::new(WsConcurrencyInner {
                global_cap: global_cap.max(1),
                per_ip_cap: per_ip_cap.max(1),
                global_count: AtomicUsize::new(0),
                per_ip: DashMap::new(),
            }),
        }
    }

    /// Try to admit a session from `ip`. On `Admitted` the caller
    /// owns one global slot and one per-IP slot, both released by
    /// [`Self::release`] (or [`Self::release_with_ip`]).
    pub fn try_admit(&self, ip: Option<IpAddr>) -> AdmitDecision {
        // Loop on the global counter so two racing upgrades cannot
        // both observe `global_count == cap - 1` and both admit.
        loop {
            let current = self.inner.global_count.load(Ordering::Relaxed);
            if current >= self.inner.global_cap {
                return AdmitDecision::GlobalFull;
            }
            if self
                .inner
                .global_count
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        // Global slot reserved. Now check the per-IP cap if a peer
        // address is known.
        if let Some(addr) = ip {
            // Loop on the per-IP counter the same way.
            loop {
                let entry = self
                    .inner
                    .per_ip
                    .entry(addr)
                    .or_insert_with(|| AtomicUsize::new(0));
                let current = entry.load(Ordering::Relaxed);
                if current >= self.inner.per_ip_cap {
                    // Roll back the global slot we just reserved;
                    // the caller surfaces the per-IP rejection.
                    self.inner.global_count.fetch_sub(1, Ordering::AcqRel);
                    return AdmitDecision::PerIpFull;
                }
                if entry
                    .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    break;
                }
            }
        }
        AdmitDecision::Admitted
    }

    /// Release the slots owned by a session whose peer IP is
    /// known. Both counters are floored at `0` so a double-release
    /// (e.g. an explicit `unregister` after the sweep already
    /// removed the entry) is a no-op rather than a wrap-around.
    pub fn release_with_ip(&self, ip: IpAddr) {
        self.release_global();
        if let Some(entry) = self.inner.per_ip.get(&ip) {
            let _ = entry.fetch_update(Ordering::AcqRel, Ordering::Relaxed, |v: usize| {
                Some(v.saturating_sub(1))
            });
            // Drop the read guard before evicting below.
            drop(entry);
        }
        self.evict_zero_entries();
    }

    /// Release only the global slot — used by sessions opened
    /// without a peer IP (in-process tests, harnesses).
    pub fn release_global(&self) {
        let _ = self.inner.global_count.fetch_update(
            Ordering::AcqRel,
            Ordering::Relaxed,
            |v: usize| Some(v.saturating_sub(1)),
        );
    }

    /// Drop every per-IP entry that just hit zero. Best-effort:
    /// keeps the map key-space bounded so a flood of distinct
    /// peer IPs does not grow the map without bound. The check
    /// runs after every `release_with_ip`; the cost is one map
    /// scan over zero-valued entries, which is cheap because
    /// those entries are uncommon.
    fn evict_zero_entries(&self) {
        self.inner
            .per_ip
            .retain(|_, v| v.load(Ordering::Relaxed) > 0);
    }

    /// Configured global cap. Exposed for tests / `/api/version`.
    pub fn global_cap(&self) -> usize {
        self.inner.global_cap
    }

    /// Configured per-IP cap. Exposed for tests / `/api/version`.
    pub fn per_ip_cap(&self) -> usize {
        self.inner.per_ip_cap
    }

    /// Current global slot usage. Exposed for tests and the
    /// `/metrics` endpoint (S-10).
    pub fn global_in_use(&self) -> usize {
        self.inner.global_count.load(Ordering::Relaxed)
    }

    /// Current per-IP slot usage for `ip`. Returns 0 for unknown
    /// IPs (no entry has been created yet). Exposed for tests.
    pub fn per_ip_in_use(&self, ip: IpAddr) -> usize {
        self.inner
            .per_ip
            .get(&ip)
            .map(|r| r.value().load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn remote_v4() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7))
    }
    fn remote_v4_other() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8))
    }
    fn remote_v6() -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1))
    }

    #[test]
    fn admit_then_release_round_trip() {
        let c = WsConcurrency::new(2, 1);
        assert_eq!(c.try_admit(Some(remote_v4())), AdmitDecision::Admitted);
        assert_eq!(c.global_in_use(), 1);
        assert_eq!(c.per_ip_in_use(remote_v4()), 1);
        c.release_with_ip(remote_v4());
        assert_eq!(c.global_in_use(), 0);
        assert_eq!(c.per_ip_in_use(remote_v4()), 0);
    }

    #[test]
    fn per_ip_cap_blocks_second_session_for_same_ip() {
        let c = WsConcurrency::new(10, 1);
        assert_eq!(c.try_admit(Some(remote_v4())), AdmitDecision::Admitted);
        // Second session from the same IP hits the per-IP cap
        // and rolls back the global slot it reserved.
        assert_eq!(c.try_admit(Some(remote_v4())), AdmitDecision::PerIpFull);
        // The first admitted session still holds its slot.
        assert_eq!(c.global_in_use(), 1, "first slot still held");
        c.release_with_ip(remote_v4());
        assert_eq!(c.global_in_use(), 0);
    }

    #[test]
    fn global_cap_blocks_when_full() {
        let c = WsConcurrency::new(2, 10);
        assert_eq!(c.try_admit(Some(remote_v4())), AdmitDecision::Admitted);
        assert_eq!(
            c.try_admit(Some(remote_v4_other())),
            AdmitDecision::Admitted
        );
        assert_eq!(c.try_admit(Some(remote_v6())), AdmitDecision::GlobalFull);
        // Global is at cap; releasing one slot allows the next
        // admission.
        c.release_with_ip(remote_v4());
        assert_eq!(c.try_admit(Some(remote_v6())), AdmitDecision::Admitted);
        c.release_with_ip(remote_v4_other());
        c.release_with_ip(remote_v6());
    }

    #[test]
    fn admit_without_ip_only_tracks_global() {
        let c = WsConcurrency::new(2, 1);
        assert_eq!(c.try_admit(None), AdmitDecision::Admitted);
        assert_eq!(c.try_admit(None), AdmitDecision::Admitted);
        assert_eq!(c.try_admit(None), AdmitDecision::GlobalFull);
        c.release_global();
        c.release_global();
        assert_eq!(c.global_in_use(), 0);
    }

    #[test]
    fn release_is_idempotent() {
        let c = WsConcurrency::new(2, 1);
        assert_eq!(c.try_admit(Some(remote_v4())), AdmitDecision::Admitted);
        c.release_with_ip(remote_v4());
        c.release_with_ip(remote_v4()); // no underflow
        c.release_global(); // no underflow
        assert_eq!(c.global_in_use(), 0);
    }

    #[test]
    fn release_evicts_zero_per_ip_entries() {
        let c = WsConcurrency::new(10, 1);
        c.try_admit(Some(remote_v4()));
        c.try_admit(Some(remote_v4_other()));
        assert_eq!(c.inner.per_ip.len(), 2);
        c.release_with_ip(remote_v4());
        c.release_with_ip(remote_v4_other());
        assert!(
            c.inner.per_ip.is_empty(),
            "zero-valued per-IP entries must be evicted (got {} keys)",
            c.inner.per_ip.len()
        );
    }
}
