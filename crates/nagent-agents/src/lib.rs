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
//! - [`egress`] — re-export of `nagent_support::egress` (the egress
//!   HTTP client used by network agents to validate URLs and fetch
//!   remote resources). The module lives in `nagent-support` so the
//!   standalone wire-client crates (`nagent-shortcut`, …) can share
//!   the SSRF policy without depending on this crate.

pub mod agents;
#[cfg(feature = "caldav-agent")]
pub mod caldav_service;
pub mod config;
/// Re-export of `nagent_support::egress` — kept as a module so every
/// `crate::egress::…` path in this crate (and every
/// `nagent_agents::egress::…` path in `nagent-server`) keeps
/// compiling unchanged.
pub mod egress {
    pub use nagent_support::egress::*;
}
#[cfg(feature = "email-agent")]
pub mod email_imap_service;
#[cfg(feature = "email-agent")]
pub mod email_smtp_service;
#[cfg(feature = "github-agent")]
pub mod github_service;
pub mod services;
#[cfg(feature = "shortcut-agent")]
pub mod shortcut_service;
pub mod tools_router;
#[cfg(feature = "x-agent")]
pub mod x_account_service;

pub use agents::{
    Agent, AgentError, AgentRegistry, AgentSummary, ConfirmationDecision, DocumentPayload,
    DocumentReadRequest, DocumentShape, DocumentSource, PageRange, SecretSink, SecretSource,
    UserContext,
};
pub use config::{
    AgentConfigs, CalDavAgentConfig, CalculateAgentConfig, DateTimeAgentConfig,
    DictionaryAgentConfig, EmailAgentConfig, GithubAgentConfig, ReadDocumentAgentConfig,
    ShortcutAgentConfig, StockAgentConfig, ToolSearchAgentConfig, UnitConvertAgentConfig,
    WeatherAgentConfig, WebFetchAgentConfig, WikipediaAgentConfig, XTimelineAgentConfig,
};
pub use tools_router::{ToolsRouter, SEARCH_TOOLS_NAME};

// Re-export the `read_document` agent unconditionally so the
// `DocumentSource` trait + the server's `StoreDocumentSource` impl
// can reference it without feature-gating every consumer. The agent
// itself is feature-gated; when the feature is off, this `pub use`
// refers to a non-existent path. Use a `cfg`-gated re-export instead.
#[cfg(feature = "read-document-agent")]
pub use agents::read_document::ReadDocumentAgent;
// Same pattern for the `search_tools` meta-agent (plan
// 1791317253718). Feature-gated so a slim build that does not need
// tool discovery drops the agent and its router cleanly.
#[cfg(feature = "tool-search-agent")]
pub use agents::tool_search::ToolSearchAgent;
pub use egress::{
    EgressClient, EgressConfig, EgressError, ValidateOk, DEFAULT_MAX_BODY_BYTES, MAX_REDIRECTS,
    MIN_TIMEOUT_MS,
};
pub use services::{
    FieldDef, FieldKind, FieldSummary, ServiceDef, ServiceRegistry, ServiceSummary,
    ECHO_ON_EDIT_NON_PASSWORD, ECHO_ON_EDIT_PASSWORD,
};
