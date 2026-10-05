//! `nagent-agents` — shared building blocks for the chat-agent subsystem.
//!
//! Crate layout:
//!
//! - [`agents`] — the `Agent` trait, `AgentError`, `UserContext`,
//!   `AgentRegistry` and every built-in agent implementation.
//!   The trait abstracts over per-user secrets (via [`SecretSource`])
//!   and per-session document access (via [`DocumentSource`]) so the
//!   crate has zero dependency on `nagent-server`, `sqlx`, or the
//!   credentials / documents internals.
//! - [`config`] — plain per-agent `*Config` structs (default +
//!   plain fields). TOML / env parsing stays in `nagent-server`'s
//!   `Config` layer because that is where the toml-mirror structs
//!   live.
//! - [`services`] — the service catalogue (`ServiceRegistry`,
//!   `ServiceDef`, `FieldKind`). Plain data types, no I/O.
//! - [`egress`] — the egress HTTP client used by network agents to
//!   validate URLs and fetch remote resources.

pub mod agents;
#[cfg(feature = "caldav-agent")]
pub mod caldav_service;
pub mod config;
pub mod egress;
pub mod services;
#[cfg(feature = "x-agent")]
pub mod x_account_service;

pub use agents::{
    Agent, AgentError, AgentRegistry, AgentSummary, ConfirmationDecision, DocumentPayload,
    DocumentSource, SecretSink, SecretSource, UserContext,
};
pub use config::{
    AgentConfigs, CalDavAgentConfig, CalculateAgentConfig, DateTimeAgentConfig,
    DictionaryAgentConfig, ReadDocumentAgentConfig, StockAgentConfig, UnitConvertAgentConfig,
    WeatherAgentConfig, WebFetchAgentConfig, WikipediaAgentConfig, XTimelineAgentConfig,
};

// Re-export the `read_document` agent unconditionally so the
// `DocumentSource` trait + the server's `StoreDocumentSource` impl
// can reference it without feature-gating every consumer. The agent
// itself is feature-gated; when the feature is off, this `pub use`
// refers to a non-existent path. Use a `cfg`-gated re-export instead.
#[cfg(feature = "read-document-agent")]
pub use agents::read_document::ReadDocumentAgent;
pub use egress::{
    EgressClient, EgressConfig, EgressError, ValidateOk, DEFAULT_MAX_BODY_BYTES, MAX_REDIRECTS,
    MIN_TIMEOUT_MS,
};
pub use services::{
    FieldDef, FieldKind, FieldSummary, ServiceDef, ServiceRegistry, ServiceSummary,
};
