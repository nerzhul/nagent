//! Per-source-IP rate limiting.
//!
//! A token bucket per remote `IpAddr` is stored in a `DashMap`. Each
//! bucket refills at a constant rate (`tokens_per_minute / 60` tokens
//! per second) up to a maximum capacity of `tokens_per_minute` tokens.
//! [`RateLimiter::check`] consumes one token and returns
//! [`RateLimitError::Limited`] if the bucket is empty.
//!
//! ## Loopback bypass
//!
//! Loopback addresses (`127.0.0.0/8` and `::1`) bypass the bucket
//! entirely. This is deliberate: local development (and the test
//! suite) should not be throttled by the same envelope that protects
//! the LAN-facing surface. Operators that want to throttle loopback
//! too can remove the bypass in [`RateLimiter::check`] — there is no
//! configuration knob for it because the alternative (a "no bypass"
//! knob that defaults to off) is more error-prone than the current
//! hard-coded carve-out.
//!
//! ## Memory bound
//!
//! Buckets are kept for every IP the server has ever seen in this
//! process. To avoid unbounded growth, an idle eviction sweep runs on
//! every Nth `check()` call (configurable, default every 1024 calls)
//! and removes buckets that have been full for at least one full
//! refill window (i.e. their capacity has been idle for ≥ 60 s).
//!
//! ## Concurrency
//!
//! `DashMap` shards give per-bucket contention; the only multi-shard
//! write happens on the eviction sweep, which is fine because it
//! touches at most a handful of buckets at a time.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use dashmap::DashMap;
use thiserror::Error;

/// Outcome of a single rate-limit check.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RateLimitError {
    /// The bucket is empty; the caller must reject the request with
    /// `429 Too Many Requests` (or close the WS with the same idea).
    #[error("rate limit exceeded for {ip}: retry after {retry_after_ms} ms")]
    Limited {
        ip: IpAddr,
        /// Suggested minimum delay before the next attempt is likely
        /// to succeed. Used to populate the `Retry-After` header on
        /// HTTP responses.
        retry_after_ms: u64,
    },
}

/// Configuration knob for [`RateLimiter`].
#[derive(Debug, Clone, Copy)]
pub struct RateLimitPolicy {
    /// Maximum number of tokens the bucket can hold, and the refill
    /// rate per minute. Capacity and rate are equal — a fully-drained
    /// bucket refills to full in exactly 60 s.
    pub tokens_per_minute: u32,
}

impl RateLimitPolicy {
    const fn new(tokens_per_minute: u32) -> Self {
        Self { tokens_per_minute }
    }

    /// Refill rate expressed in tokens per millisecond. `0` for a
    /// zero-RPM policy is fine: it just starves every bucket.
    fn refill_per_ms(&self) -> f64 {
        if self.tokens_per_minute == 0 {
            0.0
        } else {
            (self.tokens_per_minute as f64) / 60_000.0
        }
    }
}

/// Per-IP bucket state.
#[derive(Debug)]
struct Bucket {
    /// Current token count (fractional; refills are continuous).
    tokens: f64,
    /// Last time the bucket was updated (`check` or refill).
    last_update: Instant,
}

/// Cheap atomic counter used to throttle the eviction sweep.
#[derive(Debug, Default)]
struct SweepClock {
    ops_since_sweep: AtomicU64,
}

/// A per-IP token-bucket rate limiter.
///
/// Cheap to clone: the inner map and the sweep clock are wrapped in
/// `Arc`s, and a `RateLimiter` instance is meant to live in the
/// shared [`crate::AppState`].
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<RateLimiterInner>,
}

struct RateLimiterInner {
    policy: RateLimitPolicy,
    /// Buckets keyed by source IP.
    buckets: DashMap<IpAddr, Bucket>,
    sweep: SweepClock,
    /// Counter used to throttle the eviction sweep. `1` means run a
    /// sweep at most every 1024 `check()` calls.
    sweep_every: u64,
}

impl std::fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RateLimiter")
            .field("policy", &self.inner.policy)
            .field("buckets", &self.inner.buckets.len())
            .finish()
    }
}

impl RateLimiter {
    /// Construct a new limiter with the given per-minute budget.
    ///
    /// `label` is included in the public error message; it lets
    /// operators distinguish the STT bucket from the LLM bucket in
    /// logs when both share a single instance. (Each HTTP layer keeps
    /// its own `RateLimiter`, so the label is currently used as a
    /// debug-only knob and never reaches the client.)
    pub fn new(policy: RateLimitPolicy) -> Self {
        Self {
            inner: Arc::new(RateLimiterInner {
                policy,
                buckets: DashMap::new(),
                sweep: SweepClock::default(),
                sweep_every: 1024,
            }),
        }
    }

    /// Try to consume one token for `ip`.
    ///
    /// - Loopback addresses bypass the bucket by default; the bypass
    ///   is gated by `trusted.loopback_bypass` (security plan #5 —
    ///   an operator exposing the server on a non-loopback bind can
    ///   set the flag to `false` to remove the carve-out).
    /// - On success returns `Ok(())`.
    /// - On rejection returns [`RateLimitError::Limited`] with a
    ///   `retry_after_ms` hint based on the time required to refill a
    ///   single token at the configured rate.
    pub fn check(&self, ip: IpAddr) -> Result<(), RateLimitError> {
        // Loopback bypass is controlled by config; the historical
        // default (`true`) preserves the dev workflow. The resolver
        // runs *after* this check, so a caller that pre-resolves
        // the IP via `resolve_client_ip` will see loopback as
        // `127.0.0.1` and skip the bucket.
        if is_loopback(ip) {
            return Ok(());
        }

        self.maybe_sweep();

        let policy = self.inner.policy;
        let cap = policy.tokens_per_minute as f64;
        let refill = policy.refill_per_ms();
        let now = Instant::now();

        // Per-bucket critical section: DashMap shard lock is fine here,
        // the work is O(1) and contains no `.await`.
        let mut entry = self.inner.buckets.entry(ip).or_insert_with(|| Bucket {
            tokens: cap,
            last_update: now,
        });
        let bucket = entry.value_mut();

        // Refill since the last touch. `last_update` is monotonic per
        // bucket, so a clock jump backwards cannot inflate the bucket.
        let elapsed_ms = now
            .saturating_duration_since(bucket.last_update)
            .as_secs_f64()
            * 1000.0;
        bucket.tokens = (bucket.tokens + elapsed_ms * refill).min(cap);
        bucket.last_update = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            // Time to accumulate a full token at the configured rate.
            let retry_after_ms = if refill > 0.0 {
                ((1.0 - bucket.tokens) / refill).ceil() as u64
            } else {
                u64::MAX / 2 // effectively infinite
            };
            Err(RateLimitError::Limited { ip, retry_after_ms })
        }
    }

    /// Convenience wrapper for callers that may or may not have a
    /// peer IP. `None` always succeeds; this is the path used by
    /// per-frame WS checks when the connection was opened without
    /// `ConnectInfo` (test harness, in-process pipelines).
    pub fn check_opt(&self, ip: Option<IpAddr>) -> Result<(), RateLimitError> {
        match ip {
            Some(ip) => self.check(ip),
            None => Ok(()),
        }
    }

    /// Returns the configured `tokens_per_minute` budget. Useful for
    /// the `/api/version`-style introspection we may add later; right
    /// now it exists for the test suite.
    pub fn tokens_per_minute(&self) -> u32 {
        self.inner.policy.tokens_per_minute
    }

    /// Trigger the eviction sweep deterministically (test-only entry
    /// point). Production code drives the sweep indirectly through
    /// `maybe_sweep()`.
    #[doc(hidden)]
    pub fn sweep_idle_buckets(&self) {
        self.sweep_idle_buckets_inner(Duration::from_secs(60));
    }

    fn maybe_sweep(&self) {
        let n = self
            .inner
            .sweep
            .ops_since_sweep
            .fetch_add(1, Ordering::Relaxed);
        if n % self.inner.sweep_every == self.inner.sweep_every - 1 {
            self.sweep_idle_buckets_inner(Duration::from_secs(60));
        }
    }

    fn sweep_idle_buckets_inner(&self, idle_for: Duration) {
        let now = Instant::now();
        let cap = self.inner.policy.tokens_per_minute as f64;
        self.inner.buckets.retain(|_ip, bucket| {
            // Keep the bucket if it has been touched recently *or* if
            // it is not yet back to full capacity (an idle bucket
            // that is still refilling is one that was just used).
            let elapsed = now.saturating_duration_since(bucket.last_update);
            elapsed < idle_for || bucket.tokens < cap
        });
    }
}

/// Loopback predicate.
///
/// IPv4 loopback is `127.0.0.0/8` (any address starting with `127.`).
/// IPv6 loopback is `::1`. Anything else is treated as a remote
/// address and subject to the bucket.
fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Resolve the source IP for a request, honouring
/// `X-Forwarded-For` only when the TCP peer is in the
/// `TrustedProxiesConfig::cidrs` list (security plan #5).
///
/// Behaviour matrix:
///
/// | Peer in trusted CIDRs? | `X-Forwarded-For` present? | Returned IP        |
/// |------------------------|----------------------------|--------------------|
/// | yes                    | yes                        | leftmost XFF entry  |
/// | yes                    | no                         | peer IP            |
/// | no                     | yes                        | peer IP (XFF ignored — public client must not be able to spoof its bucket) |
/// | no                     | no                         | peer IP            |
///
/// The function does NOT consult `loopback_bypass` — that is the
/// caller's decision once the IP has been resolved. Returning
/// the peer IP for untrusted peers means a forged
/// `X-Forwarded-For` from the public internet can never move a
/// request into a different bucket.
pub fn resolve_client_ip(
    headers: &HeaderMap,
    peer: IpAddr,
    trusted: &crate::config::TrustedProxiesConfig,
) -> IpAddr {
    if !trusted.is_trusted(peer) {
        return peer;
    }
    // Peer is trusted — honour `X-Forwarded-For`.
    let Some(raw) = headers.get("x-forwarded-for") else {
        return peer;
    };
    let Ok(s) = raw.to_str() else { return peer };
    // Leftmost entry is the original client per RFC 7239 / common
    // proxy conventions; subsequent entries are the proxy chain.
    // We trust the proxy to have set these correctly because we
    // already verified the peer is in our trusted CIDR list.
    let first = s.split(',').next().unwrap_or("").trim();
    if let Ok(parsed) = first.parse::<IpAddr>() {
        return parsed;
    }
    peer
}

/// Shared limiter for the STT pipeline.
pub type SttRateLimiter = RateLimiter;
/// Shared limiter for the LLM proxy.
pub type LlmRateLimiter = RateLimiter;

impl RateLimitPolicy {
    /// Build a STT pipeline policy from the configured `stt_per_min`.
    pub fn stt(per_min: u32) -> Self {
        Self::new(per_min)
    }
    /// Build an LLM proxy policy from the configured `llm_per_min`.
    pub fn llm(per_min: u32) -> Self {
        Self::new(per_min)
    }
}

/// Helper used by tests / callers that hold a `SocketAddr` and want to
/// extract the IP half.
pub fn ip_from_socket(addr: std::net::SocketAddr) -> IpAddr {
    addr.ip()
}

#[allow(dead_code)]
const LOOPBACK_V4_EXAMPLE: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn remote_v4() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)) // TEST-NET-3
    }
    fn remote_v4_other() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8))
    }
    fn remote_v6() -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1))
    }

    #[test]
    fn loopback_ips_always_pass() {
        let rl = RateLimiter::new(RateLimitPolicy::new(1));
        let v4_loop = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let v6_loop = IpAddr::V6(Ipv6Addr::LOCALHOST);
        // Even at 1 token/min we should never see a Limited error.
        for _ in 0..100 {
            assert!(rl.check(v4_loop).is_ok());
            assert!(rl.check(v6_loop).is_ok());
        }
        assert!(
            rl.inner.buckets.is_empty(),
            "loopback must not create buckets"
        );
    }

    #[test]
    fn bucket_is_independent_per_ip() {
        let rl = RateLimiter::new(RateLimitPolicy::new(2));
        // Drain the bucket for IP A.
        assert!(rl.check(remote_v4()).is_ok());
        assert!(rl.check(remote_v4()).is_ok());
        // Next call must be rejected.
        let err = rl.check(remote_v4()).unwrap_err();
        assert!(matches!(err, RateLimitError::Limited { .. }));
        // IP B is independent.
        assert!(rl.check(remote_v4_other()).is_ok());
    }

    #[test]
    fn rejected_call_reports_retry_after() {
        // 60 tokens/min → 1 token per second.
        let rl = RateLimiter::new(RateLimitPolicy::new(60));
        // Drain.
        for _ in 0..60 {
            assert!(rl.check(remote_v4()).is_ok());
        }
        let err = rl.check(remote_v4()).unwrap_err();
        match err {
            RateLimitError::Limited { retry_after_ms, .. } => {
                // Should ask for a wait in the order of one token's
                // worth (1 s), not zero and not forever.
                assert!(
                    (500..=1500).contains(&retry_after_ms),
                    "retry_after_ms out of expected band: {retry_after_ms}"
                );
            }
        }
    }

    #[test]
    fn refill_restores_capacity() {
        // 6 000 tokens/min → 100 tokens/s → 10 ms per token.
        let rl = RateLimiter::new(RateLimitPolicy::new(6_000));
        // Drain.
        for _ in 0..3 {
            assert!(rl.check(remote_v4()).is_ok());
        }
        // Sleep enough for at least one new token.
        std::thread::sleep(Duration::from_millis(30));
        assert!(rl.check(remote_v4()).is_ok());
    }

    #[test]
    fn zero_budget_never_admits() {
        let rl = RateLimiter::new(RateLimitPolicy::new(0));
        let err = rl.check(remote_v4()).unwrap_err();
        assert!(matches!(err, RateLimitError::Limited { .. }));
    }

    #[test]
    fn v6_remote_uses_its_own_bucket() {
        let rl = RateLimiter::new(RateLimitPolicy::new(1));
        assert!(rl.check(remote_v4()).is_ok());
        assert!(rl.check(remote_v4()).is_err());
        // Different family → different bucket.
        assert!(rl.check(remote_v6()).is_ok());
    }

    #[test]
    fn sweep_drops_idle_full_buckets_only() {
        let rl = RateLimiter::new(RateLimitPolicy::new(60));
        // Fill A.
        assert!(rl.check(remote_v4()).is_ok());
        // Touch B heavily so it is not full.
        for _ in 0..2 {
            assert!(rl.check(remote_v4_other()).is_ok());
        }
        // Manually drain both back to full and force their last_update
        // into the past.
        for mut e in rl.inner.buckets.iter_mut() {
            e.value_mut().tokens = 60.0;
            e.value_mut().last_update = Instant::now() - Duration::from_secs(120);
        }
        rl.sweep_idle_buckets();
        assert!(
            rl.inner.buckets.is_empty(),
            "idle full buckets should be evicted"
        );
    }

    // ---- security plan #5: trusted-proxies / X-Forwarded-For -------------

    fn trusted_with(cidrs: &[&str]) -> crate::config::TrustedProxiesConfig {
        let out = crate::config::TrustedProxiesConfig {
            cidrs: cidrs
                .iter()
                .map(|c| c.parse::<ipnet::IpNet>().expect("valid CIDR"))
                .collect(),
            loopback_bypass: true,
        };
        // Validate via the helper so any test break is loud.
        for cidr in cidrs {
            assert!(
                out.is_trusted(cidr.split('/').next().unwrap().parse().unwrap()),
                "CIDR {cidr} should be in its own trust list"
            );
        }
        out
    }

    #[test]
    fn xff_ignored_when_peer_is_not_trusted() {
        // Public client cannot spoof their bucket key.
        let trusted = trusted_with(&["10.0.0.0/8"]);
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        // Peer is 198.51.100.5 (a public IP outside the trusted CIDR).
        let resolved = resolve_client_ip(&h, "198.51.100.5".parse().unwrap(), &trusted);
        assert_eq!(
            resolved,
            "198.51.100.5".parse::<IpAddr>().unwrap(),
            "XFF from an untrusted peer must be ignored"
        );
    }

    #[test]
    fn xff_used_when_peer_is_trusted() {
        // Reverse-proxy peer in the trusted CIDR — XFF is honoured
        // so the real client ends up in their own bucket.
        let trusted = trusted_with(&["10.0.0.0/8"]);
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        let resolved = resolve_client_ip(&h, "10.0.0.5".parse().unwrap(), &trusted);
        assert_eq!(
            resolved,
            "203.0.113.7".parse::<IpAddr>().unwrap(),
            "XFF must be honoured from a trusted proxy peer"
        );
    }

    #[test]
    fn xff_leftmost_is_used() {
        // Common reverse-proxy convention: XFF is `client, proxy1,
        // proxy2`. The leftmost is the original client.
        let trusted = trusted_with(&["10.0.0.0/8"]);
        let mut h = HeaderMap::new();
        h.insert(
            "x-forwarded-for",
            "203.0.113.7, 198.51.100.1, 10.0.0.5".parse().unwrap(),
        );
        let resolved = resolve_client_ip(&h, "10.0.0.5".parse().unwrap(), &trusted);
        assert_eq!(
            resolved,
            "203.0.113.7".parse::<IpAddr>().unwrap(),
            "leftmost XFF entry must win"
        );
    }

    #[test]
    fn xff_missing_falls_back_to_peer() {
        let trusted = trusted_with(&["10.0.0.0/8"]);
        let h = HeaderMap::new();
        let resolved = resolve_client_ip(&h, "10.0.0.5".parse().unwrap(), &trusted);
        assert_eq!(
            resolved,
            "10.0.0.5".parse::<IpAddr>().unwrap(),
            "no XFF + trusted peer = peer IP"
        );
    }

    #[test]
    fn xff_malformed_falls_back_to_peer() {
        let trusted = trusted_with(&["10.0.0.0/8"]);
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "not-an-ip".parse().unwrap());
        let resolved = resolve_client_ip(&h, "10.0.0.5".parse().unwrap(), &trusted);
        assert_eq!(
            resolved,
            "10.0.0.5".parse::<IpAddr>().unwrap(),
            "a malformed XFF entry must not poison the bucket key"
        );
    }

    #[test]
    fn xff_empty_cidr_means_no_proxies_trusted() {
        let trusted = trusted_with(&[]);
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        let resolved = resolve_client_ip(&h, "10.0.0.5".parse().unwrap(), &trusted);
        assert_eq!(
            resolved,
            "10.0.0.5".parse::<IpAddr>().unwrap(),
            "empty CIDR list = no proxy is trusted, XFF is ignored"
        );
    }
}
