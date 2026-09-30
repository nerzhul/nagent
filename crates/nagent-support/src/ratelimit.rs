//! Shared bookkeeping for the in-process rate limiters.
//!
//! Two limiters live in this crate today:
//!
//! - `nagent_server::rate_limit::RateLimiter` — per-source-IP token
//!   bucket for STT and LLM traffic.
//! - `nagent_server::auth::login_rate_limit::LoginRateLimiter` — three
//!   bucket families for login attempts (`(email, ip)`, per-email,
//!   per-IP) with fixed-window counters and exponential backoff.
//!
//! They share two pieces of bookkeeping:
//!
//! - A [`SweepClock`] that throttles the eviction sweep so it runs
//!   at most once every N operations, regardless of request rate.
//! - A `last_update`-based eviction predicate ("keep entries that
//!   have been touched in the last `idle_for`").
//!
//! The bucket math itself is meaningfully different (continuous
//! token refill vs discrete window + backoff), so this module
//! intentionally does NOT unify the bucket types. Each limiter
//! keeps its own state and reaches for [`SweepClock`] /
//! [`should_keep_during_idle_eviction`] when it needs the shared
//! pieces.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Default interval between eviction sweeps, expressed as a count
/// of `check`-class operations. Both existing limiters use this
/// value; it is exposed as a `pub` constant so the per-crate
/// callers can reference it.
pub const DEFAULT_SWEEP_EVERY: u64 = 1024;

/// Atomic counter that gates a periodic sweep.
///
/// Bumped once per `check`-class operation; [`SweepClock::tick`]
/// returns `true` exactly once every `sweep_every` calls so the
/// caller can run a sweep. Cheap to clone (`Arc`-backed via the
/// owning limiter).
#[derive(Debug, Default)]
pub struct SweepClock {
    ops_since_sweep: AtomicU64,
}

impl SweepClock {
    pub fn new() -> Self {
        Self {
            ops_since_sweep: AtomicU64::new(0),
        }
    }

    /// Increment the counter; return `true` when the caller should
    /// run a sweep (every `sweep_every` calls).
    pub fn tick(&self, sweep_every: u64) -> bool {
        if sweep_every == 0 {
            // Defensive: a 0 sweep_every would cause a divide-by-zero
            // and trigger a sweep on every call. We treat it as "no
            // automatic sweeping" so a misconfiguration does not
            // stall the request hot path on map retention.
            return false;
        }
        let n = self.ops_since_sweep.fetch_add(1, Ordering::Relaxed);
        n % sweep_every == sweep_every - 1
    }

    /// Reset the counter so the next `tick` is at the beginning of
    /// the cycle. Test-only hook; production code drives the
    /// counter from the hot path.
    #[cfg(test)]
    pub fn reset(&self) {
        self.ops_since_sweep.store(0, Ordering::Relaxed);
    }
}

/// Eviction predicate shared by the limiters: keep a bucket that
/// was touched recently (or that is still draining). Mirrors the
/// "keep while refilling" semantics used by both limiters today.
///
/// `now` is sampled once by the caller; `last_update` is the bucket
/// metadata; `idle_for` is the idle threshold.
#[inline]
pub fn should_keep_during_idle_eviction(
    now: Instant,
    last_update: Instant,
    idle_for: std::time::Duration,
) -> bool {
    now.saturating_duration_since(last_update) < idle_for
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn tick_returns_true_every_sweep_every_calls() {
        let clock = SweepClock::new();
        // First 1023 calls must not trigger a sweep.
        for i in 0..1023 {
            assert!(
                !clock.tick(1024),
                "tick #{i} should not trigger a sweep (1024 calls / sweep)"
            );
        }
        // The 1024th call (n = 1023) is the trigger.
        assert!(
            clock.tick(1024),
            "tick #1024 (n = 1023) must trigger the sweep"
        );
        // 1025th call (n = 1024 after wrap) does not.
        assert!(!clock.tick(1024));
    }

    #[test]
    fn tick_with_zero_sweep_every_is_a_no_op() {
        // Defensive: a misconfigured sweep_every = 0 must NOT cause
        // every call to trigger a sweep (that would stall the hot
        // path on DashMap retention).
        let clock = SweepClock::new();
        for _ in 0..10 {
            assert!(!clock.tick(0));
        }
    }

    #[test]
    fn reset_starts_a_fresh_cycle() {
        let clock = SweepClock::new();
        for _ in 0..1023 {
            clock.tick(1024);
        }
        clock.reset();
        // First call after reset is n = 0, no sweep.
        assert!(!clock.tick(1024));
    }

    #[test]
    fn idle_eviction_predicate_keeps_recent_entries() {
        let now = Instant::now();
        let recent = now - Duration::from_secs(10);
        let ancient = now - Duration::from_secs(120);
        assert!(should_keep_during_idle_eviction(
            now,
            recent,
            Duration::from_secs(60)
        ));
        assert!(!should_keep_during_idle_eviction(
            now,
            ancient,
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn idle_eviction_predicate_handles_backwards_clock() {
        // `saturating_duration_since` returns zero on a backwards
        // jump, so the predicate must keep the entry. Without this
        // guarantee a NTP step backwards would wipe every bucket.
        let now = Instant::now();
        let future = now + Duration::from_secs(60);
        assert!(should_keep_during_idle_eviction(
            now,
            future,
            Duration::from_secs(60)
        ));
    }
}
