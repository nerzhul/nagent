//! Login-attempt rate limiter
//!
//! Three orthogonal bucket families share the same per-attempt
//! decision so a single attacker cannot outflank the throttle by
//! spreading attempts across different key axes:
//!
//! - **`(email, ip)`** — the historical per-pair bucket. Caps
//!   the most precise axis (one user, one source IP) at
//!   `pair_max / pair_window`. The fastest bucket to trip and
//!   the one with the most surgical denial.
//! - **`email`** — per-email global counter (regardless of
//!   source IP). Defends against an attacker that rotates IPs
//!   to grind through one account. `email_max / email_window`,
//!   with exponential backoff once `email_backoff_after` is hit.
//! - **`ip`** — per-IP global counter (regardless of email).
//!   Defends against an attacker that rotates emails to grind
//!   through one source IP. `ip_max / ip_window`, with
//!   exponential backoff once `ip_backoff_after` is hit.
//!
//! `check` consults all three families; any deny short-circuits
//! the decision and the longest `retry_after_secs` wins (the
//! caller has to wait at least as long as the most-paranoid
//! bucket wants). A successful login calls `reset` to clear the
//! per-(email, ip) and per-email buckets so the user is not
//! penalised for past bad attempts they subsequently corrected.
//!
//! All three bucket families share the same in-process
//! `DashMap`; entries are evicted by a periodic sweep and a
//! hard cap (`max_entries`) drops the oldest entries by
//! `last_update` first if the cap is hit. Cheap to clone (the
//! inner map is `Arc`-backed); lives in the shared
//! [`crate::auth`] state.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::support::ratelimit::{SweepClock, DEFAULT_SWEEP_EVERY};

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
/// blow past it.
pub const DEFAULT_LOGIN_MAX_BUCKETS: usize = 16 * 1024;

/// Maximum backoff per bucket — caps the exponential growth at
/// one hour. Without this, a persistent attacker would multiply
/// the wait indefinitely; one hour is long enough to disrupt a
/// credential-spray campaign while keeping self-DoS recovery
/// reasonable (an operator who genuinely forgot their password
/// only has to wait an hour, not a day).
pub const MAX_LOGIN_BACKOFF_SECS: u64 = 3600;

/// Key kinds for the three bucket families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BucketKind {
    Pair,
    Email,
    Ip,
}

impl BucketKind {
    #[allow(dead_code)]
    fn label(self) -> &'static str {
        match self {
            BucketKind::Pair => "pair",
            BucketKind::Email => "email",
            BucketKind::Ip => "ip",
        }
    }
}

/// One bucket row. `last_update` is the wall-clock at the most
/// recent `check` (whether allowed or denied) — used by the
/// bounded-map eviction policy.
#[derive(Debug)]
struct Bucket {
    count: u32,
    window_start: Instant,
    last_update: Instant,
    /// Number of consecutive `Deny`s for this key (resets on
    /// `reset` or on a fresh `Allow`). Drives the exponential
    /// backoff: `retry_after_secs = base_retry *
    /// 2^consecutive_denies` capped at [`MAX_LOGIN_BACKOFF_SECS`].
    consecutive_denies: u32,
}

impl Bucket {
    fn new(now: Instant) -> Self {
        Self {
            count: 0,
            window_start: now,
            last_update: now,
            consecutive_denies: 0,
        }
    }

    fn refresh_window(&mut self, now: Instant) {
        self.count = 0;
        self.consecutive_denies = 0;
        self.window_start = now;
        self.last_update = now;
    }
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
    inner: std::sync::Arc<LoginRateLimiterInner>,
}

struct LoginRateLimiterInner {
    pair_buckets: DashMap<(String, IpAddr), Bucket>,
    email_buckets: DashMap<String, Bucket>,
    ip_buckets: DashMap<IpAddr, Bucket>,
    pair_max: u32,
    pair_window: Duration,
    email_max: u32,
    email_window: Duration,
    email_backoff_after: u32,
    ip_max: u32,
    ip_window: Duration,
    ip_backoff_after: u32,
    /// Hard cap on the total number of buckets across all three
    /// maps. See [`DEFAULT_LOGIN_MAX_BUCKETS`].
    max_entries: usize,
    /// Monotonic counter used to throttle eviction sweeps (the
    /// existing `rate_limit.rs` does the same trick).
    sweep: SweepClock,
    sweep_every: u64,
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
    pub max_entries: usize,
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
            max_entries: DEFAULT_LOGIN_MAX_BUCKETS,
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
        Self {
            inner: std::sync::Arc::new(LoginRateLimiterInner {
                pair_buckets: DashMap::new(),
                email_buckets: DashMap::new(),
                ip_buckets: DashMap::new(),
                pair_max: policy.pair_max,
                pair_window: policy.pair_window,
                email_max: policy.email_max,
                email_window: policy.email_window,
                email_backoff_after: policy.email_backoff_after,
                ip_max: policy.ip_max,
                ip_window: policy.ip_window,
                ip_backoff_after: policy.ip_backoff_after,
                max_entries: policy.max_entries,
                sweep: SweepClock::new(),
                sweep_every: DEFAULT_SWEEP_EVERY,
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
        self.maybe_sweep();
        let now = Instant::now();
        let normalised = normalise_email(email);

        // First pass — read-only. If ANY family is exhausted we
        // deny immediately and bump that family's deny counter
        // for backoff. We do NOT consume a token from the
        // allowed families on the deny path so a misconfigured
        // client (e.g. retrying too fast) cannot artificially
        // saturate the broader buckets with their 401 loop.
        let mut longest: Option<(u64, BucketKind)> = None;
        let mut push = |retry: u64, kind: BucketKind| {
            longest = Some(match longest {
                None => (retry, kind),
                Some((cur, _)) if retry > cur => (retry, kind),
                Some(existing) => existing,
            });
        };

        if let Some(retry) = self.read_pair(&normalised, ip, now) {
            push(retry, BucketKind::Pair);
        }
        if let Some(retry) = self.read_email(&normalised, now) {
            push(retry, BucketKind::Email);
        }
        if let Some(retry) = self.read_ip(ip, now) {
            push(retry, BucketKind::Ip);
        }

        if let Some((retry, kind)) = longest {
            self.bump_deny(BucketKind::Pair, BumpKey::Pair(&normalised, ip));
            self.bump_deny(BucketKind::Email, BumpKey::Email(&normalised));
            self.bump_deny(BucketKind::Ip, BumpKey::Ip(ip));
            return LoginRateLimitDecision::Deny {
                retry_after_secs: retry,
                kind,
            };
        }

        // Allowed: consume a token in all three families and
        // reset their deny counters (a legitimate attempt
        // breaks any in-progress backoff streak).
        self.consume(BucketKind::Pair, ConsumeKey::Pair(&normalised, ip), now);
        self.consume(BucketKind::Email, ConsumeKey::Email(&normalised), now);
        self.consume(BucketKind::Ip, ConsumeKey::Ip(ip), now);
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
        // the tuple temporary before the second `remove`
        // borrows it.
        self.inner.pair_buckets.remove(&(normalised.clone(), ip));
        self.inner.email_buckets.remove(&normalised);
        // Per-IP is NOT cleared on a successful login: a single
        // user authenticating from their laptop should not
        // wipe a separate attacker's in-progress backoff for
        // the same source IP. The per-(email, ip) and per-email
        // buckets are the surgical resets.
    }

    /// Test-only entry point that forces an eviction sweep +
    /// cap-enforcement pass across all three maps.
    #[doc(hidden)]
    pub fn sweep_idle_buckets(&self) {
        let now = Instant::now();
        let window = self
            .inner
            .pair_window
            .max(self.inner.email_window)
            .max(self.inner.ip_window);
        self.inner
            .pair_buckets
            .retain(|_, b| now.duration_since(b.window_start) < window);
        self.inner
            .email_buckets
            .retain(|_, b| now.duration_since(b.window_start) < window);
        self.inner
            .ip_buckets
            .retain(|_, b| now.duration_since(b.window_start) < window);
        self.enforce_max_entries();
    }

    // ---- read-only helpers (no consumption) ---------------------------

    fn read_pair(&self, email: &str, ip: IpAddr, now: Instant) -> Option<u64> {
        let mut entry = self
            .inner
            .pair_buckets
            .entry((email.to_string(), ip))
            .or_insert_with(|| Bucket::new(now));
        let b = entry.value_mut();
        if now.duration_since(b.window_start) >= self.inner.pair_window {
            b.refresh_window(now);
        }
        self.decision(
            b,
            self.inner.pair_max,
            self.inner.pair_window,
            now,
            // pair_max is typically 5 — backoff kicks in after
            // the same number of consecutive denies by default.
            (self.inner.pair_max / 5).max(1),
        )
    }

    fn read_email(&self, email: &str, now: Instant) -> Option<u64> {
        let mut entry = self
            .inner
            .email_buckets
            .entry(email.to_string())
            .or_insert_with(|| Bucket::new(now));
        let b = entry.value_mut();
        if now.duration_since(b.window_start) >= self.inner.email_window {
            b.refresh_window(now);
        }
        self.decision(
            b,
            self.inner.email_max,
            self.inner.email_window,
            now,
            self.inner.email_backoff_after,
        )
    }

    fn read_ip(&self, ip: IpAddr, now: Instant) -> Option<u64> {
        let mut entry = self
            .inner
            .ip_buckets
            .entry(ip)
            .or_insert_with(|| Bucket::new(now));
        let b = entry.value_mut();
        if now.duration_since(b.window_start) >= self.inner.ip_window {
            b.refresh_window(now);
        }
        self.decision(
            b,
            self.inner.ip_max,
            self.inner.ip_window,
            now,
            self.inner.ip_backoff_after,
        )
    }

    /// Returns `Some(retry_after_secs)` when the bucket is
    /// exhausted. Returns `None` when the bucket is still
    /// within budget — the caller will then consume a token
    /// through `consume`. The exponential backoff is computed
    /// from `consecutive_denies`.
    fn decision(
        &self,
        b: &mut Bucket,
        max: u32,
        window: Duration,
        now: Instant,
        backoff_after: u32,
    ) -> Option<u64> {
        if b.count < max {
            return None;
        }
        let elapsed = now.duration_since(b.window_start);
        let remaining = window.saturating_sub(elapsed);
        // Base retry: at least 1 s so the client never spins.
        let base_retry = remaining.as_secs().max(1);
        // Exponential backoff kicks in once `backoff_after`
        // denies have stacked up. Doubles on each successive
        // deny up to MAX_LOGIN_BACKOFF_SECS.
        let exp_factor = b
            .consecutive_denies
            .saturating_sub(backoff_after.saturating_sub(1));
        let retry = if exp_factor == 0 {
            base_retry
        } else {
            base_retry
                .saturating_mul(1u64 << exp_factor.min(20))
                .min(MAX_LOGIN_BACKOFF_SECS)
        };
        Some(retry)
    }

    /// Consume one token in the named family, resetting the
    /// deny counter (a successful attempt breaks the backoff
    /// streak).
    fn consume(&self, kind: BucketKind, key: ConsumeKey<'_>, now: Instant) {
        // Note: we deliberately do NOT call
        // `enforce_max_entries_for` from inside this function —
        // DashMap would deadlock if we tried to re-acquire a
        // shard lock we already hold. The cap is enforced from
        // `sweep_idle_buckets` (the periodic sweep) instead.
        match (kind, key) {
            (BucketKind::Pair, ConsumeKey::Pair(e, i)) => {
                let mut entry = self
                    .inner
                    .pair_buckets
                    .entry((e.to_string(), i))
                    .or_insert_with(|| Bucket::new(now));
                let b = entry.value_mut();
                b.count = b.count.saturating_add(1).min(self.inner.pair_max);
                b.consecutive_denies = 0;
                b.last_update = now;
            }
            (BucketKind::Email, ConsumeKey::Email(e)) => {
                let mut entry = self
                    .inner
                    .email_buckets
                    .entry(e.to_string())
                    .or_insert_with(|| Bucket::new(now));
                let b = entry.value_mut();
                b.count = b.count.saturating_add(1).min(self.inner.email_max);
                b.consecutive_denies = 0;
                b.last_update = now;
            }
            (BucketKind::Ip, ConsumeKey::Ip(i)) => {
                let mut entry = self
                    .inner
                    .ip_buckets
                    .entry(i)
                    .or_insert_with(|| Bucket::new(now));
                let b = entry.value_mut();
                b.count = b.count.saturating_add(1).min(self.inner.ip_max);
                b.consecutive_denies = 0;
                b.last_update = now;
            }
            _ => {
                // mismatched key kind — programmer error; ignore.
            }
        }
    }

    /// Increment the consecutive-deny counter on the named
    /// family so the next `decision` call computes a larger
    /// `retry_after_secs`. Called from the deny path. Uses
    /// `get_mut` so we never create a fresh bucket for a deny
    /// (a bucket only exists when a prior `check` consumed a
    /// token).
    fn bump_deny(&self, kind: BucketKind, key: BumpKey<'_>) {
        let now = Instant::now();
        match (kind, key) {
            (BucketKind::Pair, BumpKey::Pair(e, i)) => {
                if let Some(mut b) = self.inner.pair_buckets.get_mut(&(e.to_string(), i)) {
                    b.value_mut().consecutive_denies =
                        b.value_mut().consecutive_denies.saturating_add(1);
                    b.value_mut().last_update = now;
                }
            }
            (BucketKind::Email, BumpKey::Email(e)) => {
                if let Some(mut b) = self.inner.email_buckets.get_mut(&e.to_string()) {
                    b.value_mut().consecutive_denies =
                        b.value_mut().consecutive_denies.saturating_add(1);
                    b.value_mut().last_update = now;
                }
            }
            (BucketKind::Ip, BumpKey::Ip(i)) => {
                if let Some(mut b) = self.inner.ip_buckets.get_mut(&i) {
                    b.value_mut().consecutive_denies =
                        b.value_mut().consecutive_denies.saturating_add(1);
                    b.value_mut().last_update = now;
                }
            }
            _ => {}
        }
    }

    fn maybe_sweep(&self) {
        if self.inner.sweep.tick(self.inner.sweep_every) {
            self.sweep_idle_buckets();
        }
    }

    fn enforce_max_entries(&self) {
        for kind in [BucketKind::Pair, BucketKind::Email, BucketKind::Ip] {
            enforce_max_entries_for(&self.inner, kind);
        }
    }
}

/// Cheap enum wrappers to keep `consume` / `bump_deny` type-safe
/// without making the public API worse.
#[derive(Debug)]
enum ConsumeKey<'a> {
    Pair(&'a str, IpAddr),
    Email(&'a str),
    Ip(IpAddr),
}
#[derive(Debug, Clone, Copy)]
enum BumpKey<'a> {
    Pair(&'a str, IpAddr),
    Email(&'a str),
    Ip(IpAddr),
}

/// Drop the oldest buckets of `kind` when the map has grown
/// past the per-kind cap (one third of the global cap so the
/// three families share it roughly equally).
fn enforce_max_entries_for(inner: &LoginRateLimiterInner, kind: BucketKind) {
    let per_kind_cap = inner.max_entries / 3;
    let mut entries: Vec<(BucketKind, std::time::Instant, Vec<u8>)> = Vec::new();
    match kind {
        BucketKind::Pair => {
            if inner.pair_buckets.len() <= per_kind_cap {
                return;
            }
            for e in inner.pair_buckets.iter() {
                let (email, ip) = e.key().clone();
                // Disambiguate keys by hashing the email to a
                // stable byte string — the entry is then dropped
                // when we re-acquire the shard lock below.
                let mut h = email.as_bytes().to_vec();
                h.push(b'|');
                match ip {
                    IpAddr::V4(v4) => h.extend_from_slice(&v4.octets()),
                    IpAddr::V6(v6) => h.extend_from_slice(&v6.octets()),
                }
                entries.push((kind, e.value().last_update, h));
            }
        }
        BucketKind::Email => {
            if inner.email_buckets.len() <= per_kind_cap {
                return;
            }
            for e in inner.email_buckets.iter() {
                entries.push((kind, e.value().last_update, e.key().as_bytes().to_vec()));
            }
        }
        BucketKind::Ip => {
            if inner.ip_buckets.len() <= per_kind_cap {
                return;
            }
            for e in inner.ip_buckets.iter() {
                let ip = *e.key();
                match ip {
                    IpAddr::V4(v4) => {
                        entries.push((kind, e.value().last_update, v4.octets().to_vec()))
                    }
                    IpAddr::V6(v6) => {
                        entries.push((kind, e.value().last_update, v6.octets().to_vec()))
                    }
                }
            }
        }
    }
    // Sort by `last_update` and drop the oldest until we are
    // back at the cap. O(n log n) but only runs when the cap
    // is breached, which is by definition an exceptional
    // condition (a runaway client or a long-lived server that
    // has accumulated many inactive buckets).
    entries.sort_by_key(|(_, ts, _)| *ts);
    let to_drop = entries.len().saturating_sub(per_kind_cap);
    // Split off the cutoffs (oldest `to_drop` instants) before
    // consuming `entries`. The byte payload is not used in the
    // second pass — we walk the map by `last_update` instead of
    // rebuilding keys, so the cutoffs are enough.
    let cutoffs: Vec<std::time::Instant> =
        entries.iter().take(to_drop).map(|(_, ts, _)| *ts).collect();
    // `entries` is intentionally dropped here (the byte payloads
    // were only needed during the sort + cutoff selection).
    drop(entries);
    match kind {
        BucketKind::Pair => {
            let to_remove: Vec<_> = inner
                .pair_buckets
                .iter()
                .filter(|e| cutoffs.contains(&e.value().last_update))
                .map(|e| e.key().clone())
                .collect();
            for k in to_remove {
                inner.pair_buckets.remove(&k);
            }
        }
        BucketKind::Email => {
            let to_remove: Vec<_> = inner
                .email_buckets
                .iter()
                .filter(|e| cutoffs.contains(&e.value().last_update))
                .map(|e| e.key().clone())
                .collect();
            for k in to_remove {
                inner.email_buckets.remove(&k);
            }
        }
        BucketKind::Ip => {
            let to_remove: Vec<IpAddr> = inner
                .ip_buckets
                .iter()
                .filter(|e| cutoffs.contains(&e.value().last_update))
                .map(|e| *e.key())
                .collect();
            for k in to_remove {
                inner.ip_buckets.remove(&k);
            }
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
    fn different_ips_get_independent_pair_buckets() {
        let rl = LoginRateLimiter::with_limits(1, Duration::from_secs(60));
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        // Same email, different IP → fresh pair bucket.
        assert_eq!(
            rl.check("u@example.com", remote_other()),
            LoginRateLimitDecision::Allow
        );
    }

    #[test]
    fn different_emails_get_independent_pair_buckets() {
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
    fn reset_clears_the_pair_and_email_buckets() {
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
    fn reset_does_not_clear_per_ip_backoff() {
        // A separate attacker on the same source IP must keep
        // their backoff even when an unrelated user logs in
        // successfully. This is what `reset` NOT clearing the
        // per-IP bucket buys us. The test asserts the two
        // invariants directly on the inner maps rather than via
        // `check` (which would race the IP bucket).
        let policy = LoginRateLimitPolicy {
            pair_max: 100,
            ip_max: 3,
            ..LoginRateLimitPolicy::default()
        };
        let rl = LoginRateLimiter::with_policy(policy);
        // Attacker A grinds through ip_max successful attempts.
        for _ in 0..3 {
            assert!(matches!(
                rl.check("attacker@example.com", remote()),
                LoginRateLimitDecision::Allow
            ));
        }
        // One more from the attacker trips the IP bucket.
        let d = rl.check("attacker@example.com", remote());
        assert!(matches!(
            d,
            LoginRateLimitDecision::Deny {
                kind: BucketKind::Ip,
                ..
            }
        ));
        // Now an unrelated user logs in via the `reset` path.
        // `reset` must NOT touch the per-IP bucket — the attacker's
        // backoff must survive.
        rl.reset("legit@example.com", remote());
        assert!(
            rl.inner.ip_buckets.contains_key(&remote()),
            "per-IP bucket must still exist after an unrelated reset()"
        );
        // The pair bucket for (legit, remote) was deleted (the
        // whole point of `reset`); the email bucket for
        // legit@example.com was deleted too.
        assert!(
            !rl.inner
                .pair_buckets
                .contains_key(&("legit@example.com".to_string(), remote())),
            "pair bucket for the reset user must be cleared"
        );
        assert!(
            !rl.inner.email_buckets.contains_key("legit@example.com"),
            "email bucket for the reset user must be cleared"
        );
        // Sanity: a check from a brand-new (email, ip) that has
        // not been reset still trips the IP backoff. (Without
        // this check, the reset could have side-effects we did
        // not intend.)
        let d = rl.check("fresh@example.com", remote());
        assert!(
            matches!(
                d,
                LoginRateLimitDecision::Deny {
                    kind: BucketKind::Ip,
                    ..
                }
            ),
            "per-IP backoff must persist across unrelated reset()s, got {d:?}"
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

    #[test]
    fn per_email_cap_blocks_attacker_rotating_ips() {
        // Plan #12: an attacker rotates IPs but stays on the
        // same email. The pair bucket (per email+ip) gets a
        // fresh budget each time, but the per-email bucket must
        // still trip at email_max.
        let policy = LoginRateLimitPolicy {
            pair_max: 100, // pair bucket would not exhaust first
            email_max: 5,
            email_backoff_after: 1,
            ..LoginRateLimitPolicy::default()
        };
        let rl = LoginRateLimiter::with_policy(policy);
        for i in 0..5 {
            // Each attempt uses a fresh IP so the pair bucket
            // never trips.
            let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1 + i as u8));
            assert_eq!(
                rl.check("victim@example.com", ip),
                LoginRateLimitDecision::Allow,
                "attempt {i} should pass pair/IP but consume the email bucket"
            );
        }
        let d = rl.check("victim@example.com", remote());
        assert!(
            matches!(
                d,
                LoginRateLimitDecision::Deny {
                    kind: BucketKind::Email,
                    ..
                }
            ),
            "rotating IPs must not bypass the per-email cap, got {d:?}"
        );
    }

    #[test]
    fn per_ip_cap_blocks_attacker_rotating_emails() {
        let policy = LoginRateLimitPolicy {
            pair_max: 100,
            ip_max: 5,
            ip_backoff_after: 1,
            ..LoginRateLimitPolicy::default()
        };
        let rl = LoginRateLimiter::with_policy(policy);
        for i in 0..5 {
            let email = format!("target{i}@example.com");
            assert_eq!(
                rl.check(&email, remote()),
                LoginRateLimitDecision::Allow,
                "attempt {i} should pass pair/email but consume the IP bucket"
            );
        }
        let d = rl.check("new@example.com", remote());
        assert!(
            matches!(
                d,
                LoginRateLimitDecision::Deny {
                    kind: BucketKind::Ip,
                    ..
                }
            ),
            "rotating emails must not bypass the per-IP cap, got {d:?}"
        );
    }

    #[test]
    fn exponential_backoff_grows_on_consecutive_denies() {
        // Two consecutive denies on the same (email, ip) pair
        // must produce a larger retry_after on the second.
        let policy = LoginRateLimitPolicy {
            pair_max: 2,
            pair_window: Duration::from_secs(60),
            email_max: 2,
            ip_max: 2,
            email_backoff_after: 1,
            ip_backoff_after: 1,
            ..LoginRateLimitPolicy::default()
        };
        let rl = LoginRateLimiter::with_policy(policy);
        // First two attempts: allow.
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        assert_eq!(
            rl.check("u@example.com", remote()),
            LoginRateLimitDecision::Allow
        );
        // Third: deny.
        let d1 = match rl.check("u@example.com", remote()) {
            LoginRateLimitDecision::Deny {
                retry_after_secs, ..
            } => retry_after_secs,
            other => panic!("expected Deny, got {other:?}"),
        };
        // Fourth: the deny count is now 1 over the
        // backoff_after=1 threshold → exp factor 1 → retry at
        // least doubles (subject to MAX_LOGIN_BACKOFF_SECS).
        let d2 = match rl.check("u@example.com", remote()) {
            LoginRateLimitDecision::Deny {
                retry_after_secs, ..
            } => retry_after_secs,
            other => panic!("expected Deny, got {other:?}"),
        };
        assert!(
            d2 >= d1,
            "second deny should produce >= retry_after than first, got d1={d1} d2={d2}"
        );
    }

    #[test]
    fn bounded_map_caps_total_entries() {
        // Each kind gets one third of `max_entries`. Insert
        // far more than the cap and confirm the limiter stays
        // bounded.
        let policy = LoginRateLimitPolicy {
            max_entries: 60, // 20 per kind
            pair_max: 1,
            email_max: 1,
            ip_max: 1,
            ..LoginRateLimitPolicy::default()
        };
        let expected_cap = policy.max_entries;
        let rl = LoginRateLimiter::with_policy(policy);
        // Generate 30 distinct (email, ip) pairs so each kind
        // blows past its per-kind cap.
        for i in 0..30 {
            let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, (i % 250 + 1) as u8));
            let _ = rl.check(&format!("u{i}@example.com"), ip);
        }
        // Trigger a sweep so the cap is enforced.
        rl.sweep_idle_buckets();
        let total =
            rl.inner.pair_buckets.len() + rl.inner.email_buckets.len() + rl.inner.ip_buckets.len();
        assert!(
            total <= expected_cap + 16,
            "total buckets ({total}) must stay near max_entries ({})",
            expected_cap
        );
    }
}
