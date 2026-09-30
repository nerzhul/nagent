//! `stt/` — WebSocket STT pipeline.
//!
//! All modules here deal with the inbound `/ws` connection: framing,
//! validation, the per-session worker dispatch, the outbound
//! `ResultRouter` that forwards worker output to the right session,
//! and the watchdog that sweeps idle sessions.

pub mod result_router;
pub mod session;
pub mod validation;
pub mod watchdog;
pub mod ws_handler;
