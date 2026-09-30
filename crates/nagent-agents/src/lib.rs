//! `nagent-agents` — shared building blocks for the chat-agent subsystem.
//!
//! Plan 4.G splits the agent subsystem out of `stt-server` into
//! its own crate. This first commit moves only the modules that
//! have no deep dependency on `stt-server` internals:
//!
//! - [`services`] — the service catalogue (`ServiceRegistry`,
//!   `ServiceDef`, `FieldKind`). Plain data types, no I/O.
//! - [`egress`] — the egress HTTP client used by network agents to
//!   validate URLs and fetch remote resources.
//!
//! The `Agent` trait, `AgentError`, `AgentRegistry`,
//! `UserContext`, and the concrete agent implementations
//! (weather, stock, web_fetch, …) stay in `stt-server` for now;
//! they still have deep dependencies on `crate::credentials`
//! and `crate::config` that will be broken via trait abstraction
//! in a follow-up commit.

pub mod egress;
pub mod services;

pub use egress::{
    EgressClient, EgressConfig, EgressError, ValidateOk, DEFAULT_MAX_BODY_BYTES, MAX_REDIRECTS,
    MIN_TIMEOUT_MS,
};
pub use services::{
    FieldDef, FieldKind, FieldSummary, ServiceDef, ServiceRegistry, ServiceSummary,
};
