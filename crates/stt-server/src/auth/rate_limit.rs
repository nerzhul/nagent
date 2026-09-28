//! Login-attempt rate limiter. Counts attempts per `(email, ip)`
//! pair in a process-local `DashMap`, with a 15-minute rolling
//! window and a 5-attempt cap. Cheap to clone (the inner map is
//! `Arc`-backed); lives in the shared [`crate::auth`] state.
//!
//! Lives in `auth/rate_limit.rs` (not `crate::rate_limit.rs`) on
//! purpose — the per-IP token bucket there is keyed on a different
//! axis (network source identity, not `(email, ip)`), uses
//! different thresholds (the existing one is "120 frames/min",
//! login is "5 attempts / 15 min"), and would have to grow a
//! parallel key-space otherwise. See the planning handover §"Per-IP
//! rate limiting already exists in `rate_limit.rs`".

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Default cap: 5 attempts per 15-minute window.
pub const DEFAULT_LOGIN_MAX_ATTEMPTS: u32 = 5;
pub const DEFAULT_LOGIN_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Per-(email, ip) bucket.
#[derive(Debug)]
struct Bucket {
    count: u32,
    window_start: Instant,
}

/// Outcome of [`LoginRateLimiter::check`].
#[derive(Debug, PartialEq, Eq)]
pub enum LoginRateLimitDecision {
    /// The attempt is within budget.
    Allow,
    /// The bucket is exhausted. The `retry_after_secs` is the
    /// time until the window slides past the oldest attempt.
    Deny { retry_after_secs: u64 },
}

/// Cheap-to-clone wrapper around the per-(email, ip) map.
#[derive(Clone)]
pub struct LoginRateLimiter {
    inner: std::sync::Arc<LoginRateLimiterInner>,
}

struct LoginRateLimiterInner {
    buckets: DashMap<(String, std::net::IpAddr), Bucket>,
    max_attempts: u32,
    window: Duration,
    /// Monotonic counter used to throttle eviction sweeps (the
    /// existing `rate_limit.rs` does the same trick).
    ops_since_sweep: AtomicU64,
    sweep_every: u64,
}

impl LoginRateLimiter {
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_LOGIN_MAX_ATTEMPTS, DEFAULT_LOGIN_WINDOW)
    }

    pub fn with_limits(max_attempts: u32, window: Duration) -> Self {
        Self {
            inner: std::sync::Arc::new(LoginRateLimiterInner {
                buckets: DashMap::new(),
                max_attempts,
                window,
                ops_since_sweep: AtomicU64::new(0),
                sweep_every: 1024,
            }),
        }
    }

    /// Try to consume one token for the `(email, ip)` pair. On a
    /// fresh bucket the first attempt always succeeds; subsequent
    /// attempts inside the window consume tokens until the cap is
    /// hit, at which point [`LoginRateLimitDecision::Deny`] is
    /// returned.
    ///
    /// Loopback IPs bypass the bucket — local development and the
    /// test suite must not be throttled by the same envelope that
    /// protects the LAN-facing surface.
    pub fn check(&self, email: &str, ip: std::net::IpAddr) -> LoginRateLimitDecision {
        if ip.is_loopback() {
            return LoginRateLimitDecision::Allow;
        }
        self.maybe_sweep();
        let key = (normalise_email(email), ip);
        let now = Instant::now();
        let mut entry = self.inner.buckets.entry(key).or_insert(Bucket {
            count: 0,
            window_start: now,
        });
        let bucket = entry.value_mut();
        // Reset the bucket if the previous window has fully elapsed.
        if now.duration_since(bucket.window_start) >= self.inner.window {
            bucket.count = 0;
            bucket.window_start = now;
        }
        if bucket.count >= self.inner.max_attempts {
            let elapsed = now.duration_since(bucket.window_start);
            let remaining = self.inner.window.saturating_sub(elapsed);
            return LoginRateLimitDecision::Deny {
                retry_after_secs: remaining.as_secs().max(1),
            };
        }
        bucket.count += 1;
        LoginRateLimitDecision::Allow
    }

    /// Reset the bucket for `(email, ip)`. Called after a
    /// successful login so the user is not penalised for past bad
    /// attempts that they subsequently corrected.
    pub fn reset(&self, email: &str, ip: std::net::IpAddr) {
        self.inner.buckets.remove(&(normalise_email(email), ip));
    }

    /// Test-only entry point that forces an eviction sweep.
    #[doc(hidden)]
    pub fn sweep_idle_buckets(&self) {
        let now = Instant::now();
        self.inner
            .buckets
            .retain(|_, bucket| now.duration_since(bucket.window_start) < self.inner.window);
    }

    fn maybe_sweep(&self) {
        let n = self.inner.ops_since_sweep.fetch_add(1, Ordering::Relaxed);
        if n % self.inner.sweep_every == self.inner.sweep_every - 1 {
            self.sweep_idle_buckets();
        }
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
            .field("max_attempts", &self.inner.max_attempts)
            .field("window_secs", &self.inner.window.as_secs())
            .field("buckets", &self.inner.buckets.len())
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
        assert!(rl.inner.buckets.is_empty());
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
            LoginRateLimitDecision::Deny { retry_after_secs } => {
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
    fn different_ips_get_independent_buckets() {
        let rl = LoginRateLimiter::with_limits(1, Duration::from_secs(60));
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        // Same email, different IP → fresh bucket.
        assert_eq!(
            rl.check("u@example.com", remote_other()),
            LoginRateLimitDecision::Allow
        );
    }

    #[test]
    fn different_emails_get_independent_buckets() {
        let rl = LoginRateLimiter::with_limits(1, Duration::from_secs(60));
        assert_eq!(
            rl.check("a@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        assert_eq!(
            rl.check("b@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
    }

    #[test]
    fn reset_clears_the_bucket() {
        let rl = LoginRateLimiter::with_limits(2, Duration::from_secs(60));
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        assert!(matches!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Deny { .. }
        ));
        rl.reset("u@example.com", remote());
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
    }

    #[test]
    fn window_resets_after_elapsing() {
        let rl = LoginRateLimiter::with_limits(1, Duration::from_millis(50));
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        assert!(matches!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Deny { .. }
        ));
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow,
            "after window elapses, the bucket must reset"
        );
    }
}
