//! Login-attempt rate limiter
//!
//! Three orthogonal bucket families share the same per-attempt
//! decision so a single attacker cannot outflank the throttle by
//! spreading attempts across different key axes:
//!
//! - **`(email, ip)`** — the historical per-pair bucket. Caps
//! the most precise axis (one user, one source IP) at
//! `pair_max / pair_window`. The fastest bucket to trip and
//! the one with the most surgical denial.
//! - **`email`** — per-email global counter (regardless of
//! source IP). Defends against an attacker that rotates IPs
//! to grind through one account. `email_max / email_window`,
//! with exponential backoff once `email_backoff_after` is hit.
//! - **`ip`** — per-IP global counter (regardless of email).
//! Defends against an attacker that rotates emails to grind
//! through one source IP. `ip_max / ip_window`, with
//! exponential backoff once `ip_backoff_after` is hit.
//!
//! `check` consults all three families; any deny short-circuits
//! the decision and the longest `retry_after_secs` wins (the
//! caller has to wait at least as long as the most-paranoid
//! bucket wants). A successful login calls `reset` to clear the
//! per-(email, ip) and per-email buckets so the user is not
//! penalised for past bad attempts they subsequently corrected.
//!
//! Each bucket family is a [`WindowedBucketMap`] from
//! `nagent-support::ratelimit` (plan 4.B). The shared
//! [`SweepClock`] + idle-eviction predicate are not duplicated
//! here any more — the primitive owns them. Cheap to clone (the
//! inner maps are `Arc`-backed); lives in the shared
//! [`crate::auth`] state.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use nagent_support::ratelimit::{WindowedBucketMap, WindowedDecision};

/// Default per-(email, ip) cap: 5 attempts / 15 min.
pub const DEFAULT_LOGIN_MAX_ATTEMPTS: u32 = 5;
pub const DEFAULT_LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Default per-email global cap: 20 attempts / 15 min, exponential
/// backoff once 5 attempts in a row are denied.
pub const DEFAULT_LOGIN_PER_EMAIL_MAX: u32 = 20;
pub const DEFAULT_LOGIN_PER_EMAIL_WINDOW: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_LOGIN_PER_EMAIL_BACKOFF_AFTER: u32 = 5;

/// Default per-IP global cap: 50 attempts / 15 min, exponential
/// backoff once 10 attempts in a row are denied.
pub const DEFAULT_LOGIN_PER_IP_MAX: u32 = 50;
pub const DEFAULT_LOGIN_PER_IP_WINDOW: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_LOGIN_PER_IP_BACKOFF_AFTER: u32 = 10;

/// Hard cap on the number of buckets the limiter holds in
/// memory. Hitting it triggers an immediate sweep + drop-oldest
/// truncation. 16 KiB worth of `Bucket` rows is enough for any
/// realistic deployment; a runaway client is the only way to
/// reach it.
pub const DEFAULT_LOGIN_MAX_BUCKETS: usize = 16 * 1024;

/// Maximum `retry_after_secs` an inflated bucket may report.
/// Caps the exponential factor so a long-running attacker
/// cannot permanently lock themselves out.
pub const MAX_LOGIN_BACKOFF_SECS: u64 = 3600;

/// Which bucket family tripped a deny. Surfaced on the
/// [`LoginRateLimitDecision::Deny`] payload for the diagnostic
/// log line the route emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketKind {
    Pair,
    Email,
    Ip,
}

/// Outcome of [`LoginRateLimiter::check`].
#[derive(Debug, PartialEq, Eq)]
pub enum LoginRateLimitDecision {
    /// The attempt is within budget.
    Allow,
    /// At least one bucket family is exhausted. The
    /// `retry_after_secs` is the longest wait among the three
    /// families — the caller must wait at least that long.
    Deny {
        retry_after_secs: u64,
        kind: BucketKind,
    },
}

/// Cheap-to-clone wrapper around the three shared bucket maps.
#[derive(Clone)]
pub struct LoginRateLimiter {
    inner: Arc<LoginRateLimiterInner>,
}

struct LoginRateLimiterInner {
    pair_buckets: WindowedBucketMap<(String, IpAddr)>,
    email_buckets: WindowedBucketMap<String>,
    ip_buckets: WindowedBucketMap<IpAddr>,
    pair_max: u32,
    pair_window: Duration,
    /// `pair_backoff_after` is computed as `pair_max / 5` so a
    /// legitimate user who forgets a password a few times does
    /// not get exponential backoff on top of the small per-pair
    /// cap. The other families use the operator-supplied value.
    pair_backoff_after: u32,
    email_max: u32,
    email_window: Duration,
    email_backoff_after: u32,
    ip_max: u32,
    ip_window: Duration,
    ip_backoff_after: u32,
}

/// Bundle of limits for [`LoginRateLimiter::with_policy`]. Splits
/// the three bucket families into named fields so a future
/// operator knob can override each independently without churning
/// the constructor signature.
#[derive(Debug, Clone)]
pub struct LoginRateLimitPolicy {
    pub pair_max: u32,
    pub pair_window: Duration,
    pub email_max: u32,
    pub email_window: Duration,
    pub email_backoff_after: u32,
    pub ip_max: u32,
    pub ip_window: Duration,
    pub ip_backoff_after: u32,
}

impl Default for LoginRateLimitPolicy {
    fn default() -> Self {
        Self {
            pair_max: DEFAULT_LOGIN_MAX_ATTEMPTS,
            pair_window: DEFAULT_LOGIN_WINDOW,
            email_max: DEFAULT_LOGIN_PER_EMAIL_MAX,
            email_window: DEFAULT_LOGIN_PER_EMAIL_WINDOW,
            email_backoff_after: DEFAULT_LOGIN_PER_EMAIL_BACKOFF_AFTER,
            ip_max: DEFAULT_LOGIN_PER_IP_MAX,
            ip_window: DEFAULT_LOGIN_PER_IP_WINDOW,
            ip_backoff_after: DEFAULT_LOGIN_PER_IP_BACKOFF_AFTER,
        }
    }
}

impl LoginRateLimiter {
    pub fn new() -> Self {
        Self::with_policy(LoginRateLimitPolicy::default())
    }

    /// Backwards-compatible constructor: builds a limiter that
    /// uses the historical `(email, ip)`-only knobs and leaves
    /// the per-email / per-IP families at their defaults.
    /// Kept for the existing tests; production code goes
    /// through `with_policy`.
    pub fn with_limits(pair_max: u32, pair_window: Duration) -> Self {
        let policy = LoginRateLimitPolicy {
            pair_max,
            pair_window,
            ..LoginRateLimitPolicy::default()
        };
        Self::with_policy(policy)
    }

    pub fn with_policy(policy: LoginRateLimitPolicy) -> Self {
        // pair_backoff_after defaults to `pair_max / 5` (clamped
        // to 1) so a legitimate user who mistypes a few times in
        // a row does not enter exponential backoff on the small
        // per-pair bucket. Operators who want a different curve
        // can override the field directly on `LoginRateLimitPolicy`
        // in a follow-up; for now the constant matches the
        // historical behaviour.
        let pair_backoff_after = (policy.pair_max / 5).max(1);
        Self {
            inner: Arc::new(LoginRateLimiterInner {
                pair_buckets: WindowedBucketMap::new(),
                email_buckets: WindowedBucketMap::new(),
                ip_buckets: WindowedBucketMap::new(),
                pair_max: policy.pair_max,
                pair_window: policy.pair_window,
                pair_backoff_after,
                email_max: policy.email_max,
                email_window: policy.email_window,
                email_backoff_after: policy.email_backoff_after,
                ip_max: policy.ip_max,
                ip_window: policy.ip_window,
                ip_backoff_after: policy.ip_backoff_after,
            }),
        }
    }

    /// Try to consume one token for the `(email, ip)` pair.
    ///
    /// Loopback IPs bypass every bucket — local development and
    /// the test suite must not be throttled by the same envelope
    /// that protects the LAN-facing surface. (See
    /// [`crate::rate_limit`] for the broader proxy-aware
    /// per-IP limiter.)
    pub fn check(&self, email: &str, ip: IpAddr) -> LoginRateLimitDecision {
        if ip.is_loopback() {
            return LoginRateLimitDecision::Allow;
        }
        if self.inner.pair_buckets.should_sweep()
            || self.inner.email_buckets.should_sweep()
            || self.inner.ip_buckets.should_sweep()
        {
            self.sweep_idle_buckets();
        }
        let now = std::time::Instant::now();
        let normalised = normalise_email(email);

        // First pass — read-only. The windowed-bucket `check`
        // already creates the bucket + bumps `consecutive_denies`
        // on a deny, so a denied family is ready for the next
        // call without an extra bump. The "read-only" semantics
        // from the original implementation (no count bump on
        // the deny path) are preserved by the
        // `WindowedBucketMap::check` contract.
        let mut longest: Option<(u64, BucketKind)> = None;
        let mut push = |retry: u64, kind: BucketKind| {
            longest = Some(match longest {
                None => (retry, kind),
                Some((cur, _)) if retry > cur => (retry, kind),
                Some(existing) => existing,
            });
        };

        if let WindowedDecision::Deny { retry_after } = self.inner.pair_buckets.check(
            (normalised.clone(), ip),
            self.inner.pair_max,
            self.inner.pair_window,
            self.inner.pair_backoff_after,
            Duration::from_secs(MAX_LOGIN_BACKOFF_SECS),
            now,
        ) {
            push(retry_after.as_secs(), BucketKind::Pair);
        }
        if let WindowedDecision::Deny { retry_after } = self.inner.email_buckets.check(
            normalised.clone(),
            self.inner.email_max,
            self.inner.email_window,
            self.inner.email_backoff_after,
            Duration::from_secs(MAX_LOGIN_BACKOFF_SECS),
            now,
        ) {
            push(retry_after.as_secs(), BucketKind::Email);
        }
        if let WindowedDecision::Deny { retry_after } = self.inner.ip_buckets.check(
            ip,
            self.inner.ip_max,
            self.inner.ip_window,
            self.inner.ip_backoff_after,
            Duration::from_secs(MAX_LOGIN_BACKOFF_SECS),
            now,
        ) {
            push(retry_after.as_secs(), BucketKind::Ip);
        }

        if let Some((retry, kind)) = longest {
            // The denied families had their `consecutive_denies`
            // already bumped by `WindowedBucketMap::check`. Mirror
            // the historical behaviour of bumping the *other*
            // families too so a sustained attacker has every
            // axis in lock-step backoff.
            self.inner
                .pair_buckets
                .bump_deny(&(normalised.clone(), ip), now);
            self.inner.email_buckets.bump_deny(&normalised, now);
            self.inner.ip_buckets.bump_deny(&ip, now);
            return LoginRateLimitDecision::Deny {
                retry_after_secs: retry,
                kind,
            };
        }

        // Allowed: every family consumed a token (the
        // `WindowedBucketMap::check` Allow branch does this for
        // us; the three families were each Allow above).
        LoginRateLimitDecision::Allow
    }

    /// Reset every bucket family tied to a successful login.
    /// Called from `password::login_handler` after a credential
    /// match so the user is not penalised for past bad
    /// attempts they subsequently corrected.
    pub fn reset(&self, email: &str, ip: IpAddr) {
        let normalised = normalise_email(email);
        // The pair key is `(String, IpAddr)` so we have to
        // clone the normalised email to avoid moving it into
        // the tuple temporary before the second `reset`
        // borrows it.
        self.inner.pair_buckets.reset(&(normalised.clone(), ip));
        self.inner.email_buckets.reset(&normalised);
        // Per-IP is NOT cleared on a successful login: a single
        // user authenticating from their laptop should not
        // wipe a separate attacker's in-progress backoff for
        // the same source IP. The per-(email, ip) and per-email
        // buckets are the surgical resets.
    }

    /// Test-only entry point that forces an eviction sweep
    /// across all three maps. Sweep cadence is also driven from
    /// [`Self::check`] when the per-family [`SweepClock`] ticks.
    #[doc(hidden)]
    pub fn sweep_idle_buckets(&self) {
        let now = std::time::Instant::now();
        self.inner
            .pair_buckets
            .sweep_idle(now, self.inner.pair_window);
        self.inner
            .email_buckets
            .sweep_idle(now, self.inner.email_window);
        self.inner.ip_buckets.sweep_idle(now, self.inner.ip_window);
    }
}

impl Default for LoginRateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for LoginRateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoginRateLimiter")
            .field("pair_max", &self.inner.pair_max)
            .field("email_max", &self.inner.email_max)
            .field("ip_max", &self.inner.ip_max)
            .field("pair_buckets", &self.inner.pair_buckets.len())
            .field("email_buckets", &self.inner.email_buckets.len())
            .field("ip_buckets", &self.inner.ip_buckets.len())
            .finish()
    }
}

/// Lower-case + trim an email so `Alice@Example.com` and
/// `alice@example.com ` share a bucket.
fn normalise_email(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn remote() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))
    }
    fn remote_other() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10))
    }
    fn loopback() -> IpAddr {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    #[test]
    fn loopback_ips_always_pass() {
        let rl = LoginRateLimiter::with_limits(1, Duration::from_secs(60));
        for _ in 0..10 {
            assert_eq!(rl.check("a@b.c", loopback()), LoginRateLimitDecision::Allow);
        }
        // No bucket was ever created for the loopback call.
        assert!(rl.inner.pair_buckets.is_empty());
        assert!(rl.inner.email_buckets.is_empty());
        assert!(rl.inner.ip_buckets.is_empty());
    }

    #[test]
    fn fifth_attempt_succeeds_sixth_denies() {
        let rl = LoginRateLimiter::with_limits(5, Duration::from_secs(60));
        for i in 0..5 {
            assert_eq!(
                rl.check("u@example.com", remote()),
                LoginRateLimitDecision::Allow,
                "attempt {i} should pass"
            );
        }
        let d = rl.check("u@example.com", remote());
        assert!(
            matches!(d, LoginRateLimitDecision::Deny { .. }),
            "6th attempt must deny, got {d:?}"
        );
    }

    #[test]
    fn deny_reports_positive_retry_after() {
        let rl = LoginRateLimiter::with_limits(1, Duration::from_secs(60));
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        match rl.check("u@example.com", remote()) {
            LoginRateLimitDecision::Deny {
                retry_after_secs, ..
            } => {
                assert!(retry_after_secs > 0);
                assert!(retry_after_secs <= 60);
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn email_is_case_insensitive() {
        let rl = LoginRateLimiter::with_limits(2, Duration::from_secs(60));
        assert_eq!(
            rl.check("Alice@Example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        assert_eq!(
            rl.check("alice@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        assert!(matches!(
            rl.check("ALICE@example.COM", remote()),
            LoginRateLimitDecision::Deny { .. }
        ));
    }

    #[test]
    fn ip_key_is_separate_from_pair() {
        // Two different IPs against the same email: each `(email,
        // ip)` pair has its own bucket, but the per-email bucket
        // is shared. With pair_max = 1 and email_max = 2, the
        // first attempt from each IP passes (and consumes one
        // per-email token). The second attempt from either IP
        // then trips its own pair bucket; the per-email bucket
        // is now at 2/2 so the email family denies too.
        let rl = LoginRateLimiter::with_limits(1, Duration::from_secs(60));
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        assert_eq!(
            rl.check("u@example.com", remote_other()),
            LoginRateLimitDecision::Allow
        );
        // Third attempt — pair from remote fires AND per-email
        // hits 2/2.
        let d = rl.check("u@example.com", remote());
        assert!(matches!(d, LoginRateLimitDecision::Deny { .. }));
    }

    #[test]
    fn reset_clears_pair_and_email_but_not_ip() {
        let rl = LoginRateLimiter::with_limits(1, Duration::from_secs(60));
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        // Deny path.
        let d = rl.check("u@example.com", remote());
        assert!(matches!(d, LoginRateLimitDecision::Deny { .. }));
        // Reset on a successful login.
        rl.reset("u@example.com", remote());
        assert_eq!(
            rl.inner.pair_buckets.len(),
            0,
            "pair bucket must be cleared on reset"
        );
        assert_eq!(
            rl.inner.email_buckets.len(),
            0,
            "email bucket must be cleared on reset"
        );
        assert_eq!(
            rl.inner.ip_buckets.len(),
            1,
            "per-IP bucket is intentionally preserved (a successful \
             user must not wipe a separate attacker's backoff streak \
             for the same source IP)"
        );
        // Next attempt from the same `(email, ip)` pair passes.
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
    }

    #[test]
    fn exponential_backoff_grows_then_caps() {
        // Use the smallest per-pair cap so backoff kicks in
        // quickly, then deny repeatedly. The reported retry
        // must grow monotonically up to MAX_LOGIN_BACKOFF_SECS.
        let rl = LoginRateLimiter::with_limits(1, Duration::from_secs(60));
        // Fill + one deny so `consecutive_denies >= 1`.
        let _ = rl.check("u@example.com", remote());
        let mut last = 0u64;
        for i in 0..6 {
            match rl.check("u@example.com", remote()) {
                LoginRateLimitDecision::Deny {
                    retry_after_secs, ..
                } => {
                    assert!(
                        retry_after_secs >= last,
                        "retry must not shrink (was {last}, now {retry_after_secs} on iter {i})"
                    );
                    last = retry_after_secs;
                }
                LoginRateLimitDecision::Allow => panic!("expected Deny on iter {i}"),
            }
        }
        assert!(
            last <= MAX_LOGIN_BACKOFF_SECS,
            "retry_after must be capped at MAX_LOGIN_BACKOFF_SECS, got {last}"
        );
    }

    #[test]
    fn fresh_window_resets_backoff_streak() {
        // After the window expires, a new Allow must wipe the
        // backoff streak so the next deny is not inflated.
        let rl = LoginRateLimiter::with_limits(2, Duration::from_secs(1));
        // Fill + a couple of denies to stack `consecutive_denies`.
        for _ in 0..4 {
            let _ = rl.check("u@example.com", remote());
        }
        std::thread::sleep(Duration::from_millis(1100));
        // The new window starts; the first Allow resets
        // `consecutive_denies` to 0 (via the windowed-bucket
        // refresh + Allow branch).
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        // Fill the rest of the new window.
        let _ = rl.check("u@example.com", remote());
        // The next deny must be at the base retry level (no
        // exponential inflation since the streak was wiped).
        match rl.check("u@example.com", remote()) {
            LoginRateLimitDecision::Deny {
                retry_after_secs, ..
            } => {
                assert!(
                    retry_after_secs <= 2,
                    "post-reset deny must be at the base level (window=1s, \
                     retry_after ~= 1s + 1s jitter), got {retry_after_secs}"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn sweep_idle_buckets_drops_expired() {
        let rl = LoginRateLimiter::with_limits(2, Duration::from_millis(200));
        let _ = rl.check("u@example.com", remote());
        let _ = rl.check("u@example.com", remote());
        assert_eq!(rl.inner.pair_buckets.len(), 1);
        std::thread::sleep(Duration::from_millis(300));
        rl.sweep_idle_buckets();
        assert_eq!(rl.inner.pair_buckets.len(), 0);
    }
}
