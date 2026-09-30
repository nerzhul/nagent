//! Shared building blocks.
//!
//! Tiny in-crate home for primitives that more than one subsystem
//! needs. Plan 4.B:
//!
//! - [`ratelimit`] — sweep clock + eviction helpers shared by the
//!   two per-process rate limiters (`crate::rate_limit` and
//!   `crate::auth::login_rate_limit`). Each limiter keeps its own
//!   bucket math (continuous token refill vs fixed-window counter with
//!   exponential backoff); only the bookkeeping is shared.
//! - [`ttl_map`] — bounded `K -> V` map with per-entry TTL, an
//!   opportunistic sweep on `insert` and a periodic
//!   [`ttl_map::TtlMap::sweep_expired`]. The canonical in-memory
//!   store for every map reachable from an unauthenticated or
//!   low-privilege request (plan R2).
//! - [`cpu`] — [`cpu::run_bounded`] helper: wraps
//!   `tokio::task::spawn_blocking` behind a `tokio::sync::Semaphore`
//!   so a burst of Argon2 / PDF / file reads cannot starve the async
//!   runtime (plan R1).
//!
//! All three modules are dependency-free beyond the workspace:
//! `dashmap`, `std::time` and `tokio` are already pulled by the rest
//! of the crate.

pub mod cpu;
pub mod ratelimit;
pub mod ttl_map;
