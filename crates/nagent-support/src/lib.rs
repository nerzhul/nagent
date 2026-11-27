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
//! - [`egress`] — hardened HTTP egress client (SSRF pre-flight,
//!   hostname allow-list, timeouts, body-size budget) shared by
//!   every network agent and wire client.
//! - [`bm25`] — tiny dependency-free BM25 ranker shared by the
//!   tools router and local document-ranking callers (e.g.
//!   Shortcut epic auto-classification).

pub mod bm25;
pub mod cpu;
pub mod egress;
pub mod ratelimit;
pub mod ttl_map;
