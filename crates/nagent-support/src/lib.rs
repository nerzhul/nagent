//! Shared building blocks.
//!
//! Public surface of the `nagent-support` crate — formerly the
//! in-crate `support/` module of `stt-server`. Extracted in
//! plan 4.G so the same primitives are shared by every other
//! `nagent-*` crate without dragging in the full server.
//!
//! - [`ratelimit`] — sweep clock + eviction helpers used by every
//!   in-process rate limiter.
//! - [`ttl_map`] — bounded `K -> V` map with per-entry TTL.
//! - [`cpu`] — [`cpu::run_bounded`] helper (semaphore + blocking pool).

pub mod cpu;
pub mod ratelimit;
pub mod ttl_map;
