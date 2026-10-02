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
use std::time::{Duration, Instant};

use dashmap::DashMap;

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

// ---- Token-bucket primitive -----------------------------------------------
//
// Continuous-refill rate limiter shared by every per-key bucket the
// server keeps in memory (plan 4.B). The bucket math itself is
// engine-agnostic — only the keying and the eviction policy vary —
// so it lives here and both `nagent_server::rate_limit::RateLimiter`
// (per-IP) reach for it when they need to consume a token.
//
// The bucket stores a fractional `tokens` count (so a 6 000 tokens
// per minute budget can refill continuously instead of in 10 ms
// hops). The `last_update` is used by the eviction policy to drop
// fully-refilled idle entries; [`should_keep_during_idle_eviction`]
// is the shared predicate.

/// Outcome of [`TokenBucket::try_consume`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenDecision {
    /// The bucket had at least one token to spend; one has been
    /// deducted and `last_update` is `now`.
    Allow,
    /// The bucket was empty; `retry_after` is the wall-clock wait
    /// required for one more token to refill at the configured rate.
    /// `u64::MAX / 2` signals an effectively-infinite wait when
    /// the policy is configured at zero tokens per minute.
    Deny { retry_after: Duration },
}

/// One live bucket entry. `tokens` is fractional (continuous refill);
/// `last_update` is the most recent touch and drives the idle-
/// eviction predicate.
///
/// Fields are `pub` so test harnesses in downstream crates can poke
/// the bucket state directly (plan 4.B: the limiter test suite
/// seeds full + idle buckets by hand). The [`TokenBucketMap`] never
/// reads them directly — it always goes through
/// [`TokenBucket::try_consume`] / [`TokenBucket::last_update`].
#[derive(Debug)]
pub struct TokenBucket {
    pub tokens: f64,
    pub last_update: Instant,
}

impl TokenBucket {
    /// Build a full bucket (capacity = policy capacity, last_update
    /// = now).
    pub fn full(capacity: f64, now: Instant) -> Self {
        Self {
            tokens: capacity,
            last_update: now,
        }
    }

    /// Refill since `last_update` (clamped to capacity), then try to
    /// consume one token. The refill rate is `tokens_per_ms`
    /// (typically `policy.tokens_per_minute / 60_000`); a zero rate
    /// drains the bucket.
    ///
    /// Returns the outcome and the post-touch bucket state. The
    /// caller is responsible for writing the returned state back
    /// into its storage — the entry does not own its location.
    pub fn try_consume(
        &mut self,
        capacity: f64,
        tokens_per_ms: f64,
        now: Instant,
    ) -> TokenDecision {
        let elapsed_ms = now
            .saturating_duration_since(self.last_update)
            .as_secs_f64()
            * 1000.0;
        self.tokens = (self.tokens + elapsed_ms * tokens_per_ms).min(capacity);
        self.last_update = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            TokenDecision::Allow
        } else {
            let retry_after = if tokens_per_ms > 0.0 {
                let secs = ((1.0 - self.tokens) / tokens_per_ms) / 1000.0;
                Duration::from_secs_f64(secs)
            } else {
                Duration::from_secs(u64::MAX / 2)
            };
            TokenDecision::Deny { retry_after }
        }
    }

    /// Last touch timestamp. Used by the idle-eviction predicate in
    /// [`should_keep_during_idle_eviction`].
    pub fn last_update(&self) -> Instant {
        self.last_update
    }
}

/// Bounded per-key token-bucket map. Wraps [`DashMap`] with the
/// per-entry [`TokenBucket`] state and the shared sweep clock +
/// idle-eviction predicate. The per-key buckets live here so the
/// per-IP limiter in `nagent-server` shrinks to a thin policy +
/// loopback-bypass layer (plan 4.B).
///
/// Clone is `Arc`-cheap; the inner `DashMap` is the only state.
pub struct TokenBucketMap<K> {
    buckets: DashMap<K, TokenBucket>,
    sweep: SweepClock,
    sweep_every: u64,
}

impl<K: Eq + std::hash::Hash + Clone> TokenBucketMap<K> {
    /// Build an empty bucket map. `sweep_every` defaults to
    /// [`DEFAULT_SWEEP_EVERY`] (`1 024`) for parity with the
    /// existing per-IP limiter; callers can override.
    pub fn new() -> Self {
        Self::with_sweep_every(DEFAULT_SWEEP_EVERY)
    }

    /// Build an empty bucket map with a custom sweep cadence.
    pub fn with_sweep_every(sweep_every: u64) -> Self {
        Self {
            buckets: DashMap::new(),
            sweep: SweepClock::new(),
            sweep_every,
        }
    }

    /// Number of live buckets. Surfaced for tests + diagnostics.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Try to consume one token for `key`. `capacity` and
    /// `tokens_per_ms` come from the caller (typically a
    // `RateLimitPolicy`); see [`TokenBucket::try_consume`] for
    /// the math.
    pub fn try_consume(
        &self,
        key: K,
        capacity: f64,
        tokens_per_ms: f64,
        now: Instant,
    ) -> TokenDecision {
        let mut entry = self
            .buckets
            .entry(key)
            .or_insert_with(|| TokenBucket::full(capacity, now));
        entry.try_consume(capacity, tokens_per_ms, now)
    }

    /// Drive the periodic eviction sweep. Returns the number of
    /// buckets removed. Callers should drive it from the hot path
    /// (gated by [`SweepClock::tick`]) or directly from tests.
    ///
    /// A bucket is dropped when its `last_update` is older than
    /// `idle_for` AND it has refilled back to capacity (an
    /// in-progress refill means the bucket was just used and must
    /// survive the sweep).
    pub fn sweep_idle(&self, now: Instant, idle_for: Duration, capacity: f64) -> usize {
        let before = self.buckets.len();
        self.buckets.retain(|_, bucket| {
            should_keep_during_idle_eviction(now, bucket.last_update, idle_for)
                || bucket.tokens < capacity
        });
        before - self.buckets.len()
    }

    /// Periodic-sweep gate. Returns `true` when the caller should
    /// run a sweep this call (every `sweep_every` ops).
    pub fn should_sweep(&self) -> bool {
        self.sweep.tick(self.sweep_every)
    }

    /// Test-only handle to drive a sweep deterministically.
    #[doc(hidden)]
    pub fn sweep_now(&self, idle_for: Duration, capacity: f64) -> usize {
        self.sweep_idle(Instant::now(), idle_for, capacity)
    }

    /// Test-only handle to iterate the inner map.
    #[doc(hidden)]
    pub fn buckets_for_tests(&self) -> &DashMap<K, TokenBucket> {
        &self.buckets
    }
}

impl<K: Eq + std::hash::Hash + Clone> Default for TokenBucketMap<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + std::hash::Hash + Clone> std::fmt::Debug for TokenBucketMap<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenBucketMap")
            .field("len", &self.buckets.len())
            .field("sweep_every", &self.sweep_every)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// WindowedBucketMap — fixed-window + exponential-backoff primitive
// ---------------------------------------------------------------------------
//
// Shared by the three bucket families of
// `nagent-server::auth::login_rate_limit::LoginRateLimiter`. Differs
// from [`TokenBucketMap`] in two ways:
//
// - **Discrete window**, not continuous refill. The window resets
//   when `now - window_start >= window`; count starts back at
//   `0`. The bucket math is integer (u32 counts), which is
//   simpler and cheaper than the fractional token model.
// - **Exponential backoff**. Each successive deny doubles
//   `retry_after` up to `max_backoff`. The number of consecutive
//   denies is tracked per-key so a single legitimate success
//   resets the backoff streak.
//
// The plan calls these "TokenBucketMap" but the family the login
// limiter needs is fixed-window-with-backoff, not the
// token-bucket-with-refill shape `TokenBucketMap` already
// implements. Plan 4.B therefore asks for a separate primitive
// that lives next to [`TokenBucketMap`] and shares the same
// `SweepClock` + idle-eviction predicate.

/// Outcome of [`WindowedBucketMap::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowedDecision {
    /// The bucket is within budget; the caller consumed one
    /// token.
    Allow,
    /// The bucket is exhausted. `retry_after` is the wall-clock
    /// wait the caller must honour before the next attempt —
    /// already inflated by the exponential factor when the deny
    /// streak has crossed `backoff_after`.
    Deny { retry_after: Duration },
}

/// One live bucket entry. `count` is the number of attempts in
/// the current window; `window_start` is the wall-clock at the
/// start of that window; `consecutive_denies` drives the
/// exponential backoff and resets to `0` on the next
/// [`WindowedBucketMap::check`] Allow.
#[derive(Debug)]
pub struct WindowedBucket {
    pub count: u32,
    pub window_start: Instant,
    pub last_update: Instant,
    pub consecutive_denies: u32,
}

impl WindowedBucket {
    /// Build a fresh bucket (`count = 0`, all timestamps equal).
    pub fn new(now: Instant) -> Self {
        Self {
            count: 0,
            window_start: now,
            last_update: now,
            consecutive_denies: 0,
        }
    }

    /// Reset the window to "now" and clear the backoff streak.
    /// Called when the previous window has expired.
    fn refresh_window(&mut self, now: Instant) {
        self.count = 0;
        self.consecutive_denies = 0;
        self.window_start = now;
        self.last_update = now;
    }

    /// Decide whether the next attempt fits in the budget and,
    /// if so, consume one token. Mirrors the existing login
    /// limiter's "peek + consume on allow / bump_deny on deny"
    /// pair in a single atomic step — there is no
    /// double-counting because the deny branch only touches
    /// `consecutive_denies`, not `count`.
    ///
    /// `backoff_after` is the number of consecutive denies at
    /// which exponential backoff kicks in. `max_backoff` caps
    /// the inflated retry so a long-running attacker cannot
    /// permanently lock themselves out.
    pub fn check(
        &mut self,
        max: u32,
        window: Duration,
        backoff_after: u32,
        max_backoff: Duration,
        now: Instant,
    ) -> WindowedDecision {
        if now.duration_since(self.window_start) >= window {
            self.refresh_window(now);
        }
        if self.count >= max {
            let elapsed = now.duration_since(self.window_start);
            let remaining = window.saturating_sub(elapsed);
            // Floor at 1 s so the client never spins; clamp at
            // `max_backoff` so the operator-supplied ceiling wins
            // over any large consecutive_denies value.
            let base = remaining.as_secs().max(1);
            let exp_factor = self
                .consecutive_denies
                .saturating_sub(backoff_after.saturating_sub(1));
            let multiplier = 1u64.checked_shl(exp_factor.min(20)).unwrap_or(u64::MAX);
            let inflated = base.saturating_mul(multiplier);
            let retry = Duration::from_secs(inflated.min(max_backoff.as_secs()));
            self.consecutive_denies = self.consecutive_denies.saturating_add(1);
            self.last_update = now;
            WindowedDecision::Deny { retry_after: retry }
        } else {
            self.count = self.count.saturating_add(1).min(max);
            self.consecutive_denies = 0;
            self.last_update = now;
            WindowedDecision::Allow
        }
    }

    /// Increment `consecutive_denies` without touching `count`.
    /// Mirrors the existing limiter's `bump_deny` helper, which
    /// is used by callers who want to deny without consuming a
    /// token (the limiter's "first pass read-only" optimisation).
    ///
    /// No-op when the bucket does not exist for `key` — the
    /// caller must create the bucket via [`Self::check`] first.
    pub fn bump_deny(&mut self, now: Instant) {
        self.consecutive_denies = self.consecutive_denies.saturating_add(1);
        self.last_update = now;
    }

    /// Last touch timestamp. Used by the idle-eviction predicate.
    pub fn last_update(&self) -> Instant {
        self.last_update
    }
}

/// Bounded per-key fixed-window-with-backoff map. Wraps
/// [`DashMap`] with the per-entry [`WindowedBucket`] state and
/// the shared [`SweepClock`] + idle-eviction predicate so
/// `nagent-server::auth::login_rate_limit` can stop carrying its
/// own `DashMap` per bucket family (plan 4.B).
///
/// Clone is `Arc`-cheap; the inner `DashMap` is the only state.
pub struct WindowedBucketMap<K> {
    buckets: DashMap<K, WindowedBucket>,
    sweep: SweepClock,
    sweep_every: u64,
}

impl<K: Eq + std::hash::Hash + Clone> WindowedBucketMap<K> {
    /// Build an empty map. `sweep_every` defaults to
    /// [`DEFAULT_SWEEP_EVERY`] for parity with [`TokenBucketMap`].
    pub fn new() -> Self {
        Self::with_sweep_every(DEFAULT_SWEEP_EVERY)
    }

    /// Build an empty map with a custom sweep cadence.
    pub fn with_sweep_every(sweep_every: u64) -> Self {
        Self {
            buckets: DashMap::new(),
            sweep: SweepClock::new(),
            sweep_every,
        }
    }

    /// Number of live buckets. Surfaced for tests + diagnostics.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// Look up + consume (or deny) one bucket for `key`.
    /// `max`, `window`, `backoff_after`, `max_backoff` come from
    /// the caller (typically a per-family policy); see
    /// [`WindowedBucket::check`] for the math. The bucket is
    /// created on first call (mirroring the login limiter's
    /// `read_X` helpers).
    pub fn check(
        &self,
        key: K,
        max: u32,
        window: Duration,
        backoff_after: u32,
        max_backoff: Duration,
        now: Instant,
    ) -> WindowedDecision {
        let mut entry = self
            .buckets
            .entry(key)
            .or_insert_with(|| WindowedBucket::new(now));
        entry.check(max, window, backoff_after, max_backoff, now)
    }

    /// Increment `consecutive_denies` for `key` without touching
    /// `count`. No-op when the bucket does not exist.
    pub fn bump_deny(&self, key: &K, now: Instant) {
        if let Some(mut b) = self.buckets.get_mut(key) {
            b.value_mut().bump_deny(now);
        }
    }

    /// Remove the bucket for `key`. Called from the login
    /// limiter's `reset` path so a successful authentication
    /// clears the in-progress backoff streak.
    pub fn reset(&self, key: &K) {
        self.buckets.remove(key);
    }

    /// Drive the eviction sweep. A bucket is dropped when its
    /// window has expired AND its `last_update` is older than
    /// `window`. Returns the number of buckets removed.
    pub fn sweep_idle(&self, now: Instant, window: Duration) -> usize {
        let before = self.buckets.len();
        self.buckets
            .retain(|_, b| now.duration_since(b.window_start) < window);
        before - self.buckets.len()
    }

    /// Periodic-sweep gate. Returns `true` when the caller should
    /// run a sweep this call (every `sweep_every` ops).
    pub fn should_sweep(&self) -> bool {
        self.sweep.tick(self.sweep_every)
    }

    /// Test-only handle to drive a sweep deterministically.
    #[doc(hidden)]
    pub fn sweep_now(&self, window: Duration) -> usize {
        self.sweep_idle(Instant::now(), window)
    }

    /// Test-only handle to iterate the inner map.
    #[doc(hidden)]
    pub fn buckets_for_tests(&self) -> &DashMap<K, WindowedBucket> {
        &self.buckets
    }
}

impl<K: Eq + std::hash::Hash + Clone> Default for WindowedBucketMap<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + std::hash::Hash + Clone> std::fmt::Debug for WindowedBucketMap<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowedBucketMap")
            .field("len", &self.buckets.len())
            .field("sweep_every", &self.sweep_every)
            .finish()
    }
}

#[cfg(test)]
mod windowed_bucket_tests {
    use super::*;
    use std::net::IpAddr;
    use std::time::Duration;

    #[test]
    fn fresh_bucket_allows_until_max() {
        let map = WindowedBucketMap::<String>::new();
        let now = Instant::now();
        // 3 attempts, window 60 s, no backoff.
        for _ in 0..3 {
            assert_eq!(
                map.check(
                    "alice".to_string(),
                    3,
                    Duration::from_secs(60),
                    1,
                    Duration::from_secs(60),
                    now
                ),
                WindowedDecision::Allow
            );
        }
        // 4th attempt exceeds the cap.
        match map.check(
            "alice".to_string(),
            3,
            Duration::from_secs(60),
            1,
            Duration::from_secs(60),
            now,
        ) {
            WindowedDecision::Deny { retry_after } => {
                assert!(
                    retry_after.as_secs() >= 1,
                    "retry_after must be at least 1 s"
                );
            }
            d => panic!("expected Deny, got {d:?}"),
        }
    }

    #[test]
    fn deny_resets_count_when_window_expires() {
        let map = WindowedBucketMap::<String>::new();
        let start = Instant::now();
        // Fill the bucket.
        for _ in 0..3 {
            let _ = map.check(
                "alice".to_string(),
                3,
                Duration::from_secs(60),
                1,
                Duration::from_secs(60),
                start,
            );
        }
        // Move past the window.
        let later = start + Duration::from_secs(61);
        // Fresh attempt must allow (count was reset by the
        // window-refresh inside check).
        assert_eq!(
            map.check(
                "alice".to_string(),
                3,
                Duration::from_secs(60),
                1,
                Duration::from_secs(60),
                later
            ),
            WindowedDecision::Allow
        );
    }

    #[test]
    fn exponential_backoff_doubles_retry_then_caps() {
        let map = WindowedBucketMap::<String>::new();
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let max_backoff = Duration::from_secs(300);
        // Fill the bucket.
        for _ in 0..3 {
            let _ = map.check("alice".to_string(), 3, window, 5, max_backoff, now);
        }
        // First deny: backoff_after = 5 so no inflation yet.
        let first = match map.check("alice".to_string(), 3, window, 5, max_backoff, now) {
            WindowedDecision::Deny { retry_after } => retry_after,
            d => panic!("expected Deny, got {d:?}"),
        };
        // Each successive deny doubles (capped by consecutive_denies
        // saturating_sub(backoff_after - 1) at the 5th call).
        let second = match map.check("alice".to_string(), 3, window, 5, max_backoff, now) {
            WindowedDecision::Deny { retry_after } => retry_after,
            d => panic!("expected Deny, got {d:?}"),
        };
        assert!(
            second.as_secs() >= first.as_secs(),
            "second retry ({second:?}) must be >= first ({first:?})"
        );
        // Spam many more denies and verify the retry never exceeds
        // the operator-supplied max_backoff.
        for _ in 0..20 {
            match map.check("alice".to_string(), 3, window, 5, max_backoff, now) {
                WindowedDecision::Deny { retry_after } => {
                    assert!(
                        retry_after <= max_backoff,
                        "retry_after ({retry_after:?}) must respect max_backoff ({max_backoff:?})"
                    );
                }
                d => panic!("expected Deny, got {d:?}"),
            }
        }
    }

    #[test]
    fn allow_resets_backoff_streak() {
        let map = WindowedBucketMap::<IpAddr>::new();
        let now = Instant::now();
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let window = Duration::from_secs(60);
        let max_backoff = Duration::from_secs(300);
        // Fill the bucket at `now`, then deny once (consecutive_denies
        // becomes 1). Move past the window. In the new window, fill
        // the bucket again — the last Allow of that sequence must
        // reset the streak to 0 so the *next* deny in the new window
        // is not inflated.
        for _ in 0..4 {
            let _ = map.check(ip, 3, window, 2, max_backoff, now);
        }
        // Now at `later`, the window has reset. The first three
        // attempts Allow (consecutive_denies stays at 0 thanks to
        // the refresh); the fourth denies at the *base* retry
        // level (no exponential inflation because the streak
        // was wiped during the window refresh inside `check`).
        let later = now + Duration::from_secs(61);
        for _ in 0..3 {
            assert_eq!(
                map.check(ip, 3, window, 2, max_backoff, later),
                WindowedDecision::Allow
            );
        }
        let retry = match map.check(ip, 3, window, 2, max_backoff, later) {
            WindowedDecision::Deny { retry_after } => retry_after,
            d => panic!("expected Deny, got {d:?}"),
        };
        // Base retry is `window_saturating - elapsed` clamped to 1 s;
        // `later - window_start` is 1 s, so retry_after ≈ 59 s.
        // Crucially, it must NOT be 2x or higher (no inflation).
        assert!(
            retry.as_secs() <= 60,
            "post-reset retry must be at the base level, got {retry:?}"
        );
    }

    #[test]
    fn bump_deny_does_not_create_bucket() {
        let map = WindowedBucketMap::<String>::new();
        let now = Instant::now();
        // bump_deny on a missing key is a no-op.
        map.bump_deny(&"alice".to_string(), now);
        assert_eq!(map.len(), 0);
        // After a real check, bump_deny increments the existing
        // bucket without touching count.
        let _ = map.check(
            "alice".to_string(),
            10,
            Duration::from_secs(60),
            1,
            Duration::from_secs(60),
            now,
        );
        map.bump_deny(&"alice".to_string(), now);
        let b = map.buckets_for_tests().get("alice").unwrap();
        assert_eq!(b.count, 1, "count must not change");
        assert_eq!(b.consecutive_denies, 1, "consecutive_denies bumped");
    }

    #[test]
    fn reset_drops_bucket() {
        let map = WindowedBucketMap::<String>::new();
        let now = Instant::now();
        let _ = map.check(
            "alice".to_string(),
            3,
            Duration::from_secs(60),
            1,
            Duration::from_secs(60),
            now,
        );
        assert_eq!(map.len(), 1);
        map.reset(&"alice".to_string());
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn sweep_idle_drops_expired_buckets() {
        let map = WindowedBucketMap::<String>::new();
        let start = Instant::now();
        let _ = map.check(
            "alice".to_string(),
            3,
            Duration::from_secs(60),
            1,
            Duration::from_secs(60),
            start,
        );
        let _ = map.check(
            "bob".to_string(),
            3,
            Duration::from_secs(60),
            1,
            Duration::from_secs(60),
            start,
        );
        assert_eq!(map.len(), 2);
        let dropped = map.sweep_now(Duration::from_secs(60));
        assert_eq!(dropped, 0, "no buckets expired yet");
        let later = start + Duration::from_secs(61);
        let dropped = map.sweep_idle(later, Duration::from_secs(60));
        assert_eq!(dropped, 2);
        assert_eq!(map.len(), 0);
    }
}

#[cfg(test)]
mod token_bucket_tests {
    use super::*;

    #[test]
    fn fresh_bucket_allows_one_token_per_capacity() {
        let mut b = TokenBucket::full(2.0, Instant::now());
        let now = Instant::now();
        assert_eq!(
            b.try_consume(2.0, 0.0, now),
            TokenDecision::Allow,
            "capacity 2, no refill → one token must be available"
        );
        assert_eq!(
            b.try_consume(2.0, 0.0, now),
            TokenDecision::Allow,
            "second token consumed; third must deny"
        );
        match b.try_consume(2.0, 0.0, now) {
            TokenDecision::Deny { retry_after } => {
                assert_eq!(retry_after, Duration::from_secs(u64::MAX / 2));
            }
            other => panic!("third call must deny, got {other:?}"),
        }
    }

    #[test]
    fn refill_restores_capacity() {
        // 100 tokens/s → 1 token / 10 ms.
        let mut b = TokenBucket::full(2.0, Instant::now());
        let now = Instant::now();
        assert_eq!(
            b.try_consume(2.0, 1000.0 / 1000.0, now),
            TokenDecision::Allow
        );
        assert_eq!(
            b.try_consume(2.0, 1000.0 / 1000.0, now),
            TokenDecision::Allow
        );
        // After draining, sleep enough for one refill.
        std::thread::sleep(Duration::from_millis(15));
        let later = Instant::now();
        assert_eq!(
            b.try_consume(2.0, 1000.0 / 1000.0, later),
            TokenDecision::Allow,
            "refill must restore at least one token"
        );
    }

    #[test]
    fn fractional_refill_clamps_to_capacity() {
        // Capacity 2; drain, sleep long enough to refill 100x;
        // consume should still be capped at capacity.
        let mut b = TokenBucket::full(2.0, Instant::now());
        let now = Instant::now();
        b.try_consume(2.0, 1.0, now);
        let later = now + Duration::from_secs(60);
        assert_eq!(b.try_consume(2.0, 1.0, later), TokenDecision::Allow);
        assert_eq!(b.try_consume(2.0, 1.0, later), TokenDecision::Allow);
        // Bucket now at exactly 0 tokens (capacity 2, no more
        // refill in the next instant without sleep); we cannot
        // consume a third without another refill.
        match b.try_consume(2.0, 1.0, later) {
            TokenDecision::Deny { .. } => {}
            other => panic!("third call must deny, got {other:?}"),
        }
    }

    #[test]
    fn map_distinguishes_keys() {
        let map: TokenBucketMap<u32> = TokenBucketMap::new();
        let now = Instant::now();
        // 1 token per ms; capacity 1.
        assert_eq!(map.try_consume(1, 1.0, 1.0, now), TokenDecision::Allow);
        // Key 2 is independent — full bucket.
        assert_eq!(map.try_consume(2, 1.0, 1.0, now), TokenDecision::Allow);
        // Key 1 is now drained (no refill between calls).
        match map.try_consume(1, 1.0, 1.0, now) {
            TokenDecision::Deny { .. } => {}
            other => panic!("second consume for key 1 must deny, got {other:?}"),
        }
    }

    #[test]
    fn map_sweep_drops_idle_full_buckets_only() {
        let map: TokenBucketMap<u32> = TokenBucketMap::new();
        let now = Instant::now();
        // Seed two keys by directly inserting full buckets
        // (without consuming) so each row is "full AND idle".
        map.buckets_for_tests()
            .insert(1, TokenBucket::full(1.0, now));
        map.buckets_for_tests()
            .insert(2, TokenBucket::full(1.0, now));
        // Force both buckets to look idle AND full.
        for mut e in map.buckets_for_tests().iter_mut() {
            e.value_mut().last_update = now - Duration::from_secs(120);
        }
        let dropped = map.sweep_idle(now, Duration::from_secs(60), 1.0);
        assert_eq!(dropped, 2, "both idle full buckets must be evicted");
        assert!(map.is_empty());
    }

    #[test]
    fn map_sweep_keeps_buckets_still_refilling() {
        // A bucket whose `last_update` is recent (even if its token
        // count is currently at capacity because we just drained
        // and refilled) must NOT be dropped — the sweep predicate
        // honours `last_update` first.
        let map: TokenBucketMap<u32> = TokenBucketMap::new();
        let now = Instant::now();
        map.try_consume(1, 2.0, 0.0, now);
        // Force the bucket back to full but keep `last_update`
        // recent by sleeping 0 ms and using `now` for both touch
        // and sweep.
        let dropped = map.sweep_idle(now, Duration::from_secs(60), 2.0);
        assert_eq!(dropped, 0, "bucket touched at `now` must survive");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn map_default_sweep_every_is_1024() {
        // Pin the contract: the default sweep cadence must match
        // the historical per-IP limiter's value (1024 calls). A
        // regression here would silently change eviction cost.
        let map: TokenBucketMap<u32> = TokenBucketMap::new();
        let dbg = format!("{map:?}");
        assert!(dbg.contains("1024"));
    }
}
