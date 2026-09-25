//! Server-side chat agents.
//!
//! An `Agent` is a server-invoked tool the LLM can call mid-conversation
//! (OpenAI-style tool/function calling). The LLM proxy detects the
//! upstream `tool_calls` block, dispatches each call through the
//! [`AgentRegistry`], feeds the result back as a `role: "tool"` message,
//! and re-asks the model until it produces a final `finish_reason:
//! "stop"` reply or the round cap is hit.
//!
//! The v1 agent is [`web_fetch`] (feature-gated behind `web-agent`).
//! The trait is intentionally shaped to also accept MCP-style agents
//! later without touching the proxy, the SSE event format, or the
//! frontend.
//!
//! ## Wire format
//!
//! Agents are described to the LLM with the same `tools: [...]` shape
//! OpenAI uses (name, description, JSON-Schema parameters). To the
//! browser we surface calls and results as named SSE events:
//!
//! ```text
//! event: tool_call\n
//! data: {"id":"call_x","name":"web_fetch","args":{...},"index":0}\n
//! \n
//! event: tool_result\n
//! data: {"id":"call_x","name":"web_fetch","ok":true,"summary":"..."}\n
//! \n
//! ```
//!
//! See the [`crate::llm`] module for how these events are emitted.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

/// Errors surfaced by [`Agent::invoke`].
///
/// Mapped to HTTP responses by the proxy / direct-invoke routes so
/// callers get an informative status without internal details leaking.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// The agent refused to run because the URL or arguments violated
    /// the sandbox policy (private IPs, oversized payload, …). Surfaces
    /// as `400 Bad Request`.
    #[error("sandbox denied: {0}")]
    SandboxDenied(String),
    /// The argument JSON did not match the agent's parameter schema.
    /// Surfaces as `400 Bad Request`.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// The agent itself returned a non-recoverable failure (network
    /// error, timeout, parse error). Surfaces as `502 Bad Gateway`.
    #[error("agent failed: {0}")]
    AgentFailed(String),
    /// The upstream resource the agent tried to talk to returned an
    /// unexpected HTTP status. Surfaces as `502 Bad Gateway` with the
    /// upstream status echoed back.
    #[error("upstream returned {status}: {body}")]
    Upstream { status: u16, body: String },
    /// The response body was larger than the supplied `max_bytes`
    /// budget. The retry policy in the calling agent decides whether
    /// to retry with a larger budget; surfaces as a non-fatal
    /// signal rather than a hard `502`.
    #[error("response exceeded max_bytes={budget}")]
    ResponseExceeded { budget: usize },
}

/// One registered chat agent.
///
/// `Send + Sync` is required because `AppState` is cloned into every
/// axum handler task; the registry is shared across all of them.
#[async_trait]
pub trait Agent: Send + Sync {
    /// Stable, lowercase name used in the `tools[].function.name`
    /// field and in the `/v1/agents/:name/invoke` URL. Must be unique
    /// per registry instance.
    fn name(&self) -> &str;

    /// One-line human-readable description of what the agent does.
    /// Sent to the LLM as `tools[].function.description`.
    fn description(&self) -> &str;

    /// JSON-Schema describing the agent's arguments. Sent to the LLM
    /// as `tools[].function.parameters`. The shape should be the
    /// standard `{ "type": "object", "properties": {...}, "required":
    /// [...] }`; OpenAI-compatible models expect this format
    /// verbatim.
    fn parameters_schema(&self) -> Value;

    /// Execute the agent with the supplied arguments and return a
    /// JSON-encoded string. The string is fed back to the LLM as
    /// `role: "tool"` `content` — JSON keeps it parseable, lets the
    /// LLM pick the fields it cares about, and matches what
    /// OpenAI's Python SDK produces for a tool result.
    async fn invoke(&self, args: Value) -> Result<String, AgentError>;
}

/// Registry of every agent exposed by this server.
///
/// Cheap to clone (`Arc` inside) so it lives directly in `AppState`.
#[derive(Clone, Default)]
pub struct AgentRegistry {
    inner: Arc<AgentRegistryInner>,
}

#[derive(Default)]
struct AgentRegistryInner {
    agents: Vec<Arc<dyn Agent>>,
}

impl std::fmt::Debug for AgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRegistry")
            .field(
                "agents",
                &self
                    .inner
                    .agents
                    .iter()
                    .map(|a| a.name())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl AgentRegistry {
    /// Empty registry. Used when the `AGENTS_ENABLED` flag is off, the
    /// `web-agent` feature is off, or both — calling `.tools_schema()`
    /// on this returns `[]` so the proxy injects no `tools` field,
    /// and `/v1/agents` returns `[]`.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build a registry that contains every agent compiled in.
    ///
    /// When the `web-agent` feature is off this is equivalent to
    /// `AgentRegistry::empty()` and the registry carries zero HTTP
    /// clients, zero DNS resolvers, and zero new dependencies.
    pub fn from_config(cfg: &super::config::AgentConfig) -> Self {
        if !cfg.enabled {
            return Self::empty();
        }
        #[allow(unused_mut)]
        let mut agents: Vec<Arc<dyn Agent>> = Vec::new();
        #[cfg(feature = "web-agent")]
        {
            agents.push(Arc::new(web_fetch::WebFetchAgent::new(
                cfg.web_fetch.clone(),
            )));
        }
        #[cfg(feature = "datetime-agent")]
        {
            agents.push(Arc::new(datetime_agent::DateTimeAgent::new()));
        }
        #[cfg(feature = "weather-agent")]
        {
            agents.push(Arc::new(weather_agent::WeatherAgent::new()));
        }
        #[cfg(feature = "stock-agent")]
        {
            agents.push(Arc::new(stock_agent::StockAgent::new()));
        }
        #[cfg(not(any(
            feature = "web-agent",
            feature = "datetime-agent",
            feature = "weather-agent",
            feature = "stock-agent"
        )))]
        {
            let _ = cfg; // suppress unused warning when no agent features are on
        }
        Self {
            inner: Arc::new(AgentRegistryInner { agents }),
        }
    }

    /// Concise description used by `GET /v1/agents`. Schema details
    /// are intentionally omitted — the LLM sees the schema via the
    /// `tools` array, not the browser.
    pub fn list(&self) -> Vec<AgentSummary> {
        self.inner
            .agents
            .iter()
            .map(|a| AgentSummary {
                name: a.name().to_string(),
                description: a.description().to_string(),
            })
            .collect()
    }

    /// Look up an agent by name. Returns `None` for unknown names so
    /// the proxy can synthesise a `role: "tool"` error message and let
    /// the LLM recover gracefully.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Agent>> {
        self.inner
            .agents
            .iter()
            .find(|a| a.name() == name)
            .map(Arc::clone)
    }

    /// OpenAI-compatible `tools` array suitable for direct injection
    /// into the LLM request body. Empty when no agents are registered.
    pub fn tools_schema(&self) -> Vec<Value> {
        self.inner
            .agents
            .iter()
            .map(|a| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": a.name(),
                        "description": a.description(),
                        "parameters": a.parameters_schema(),
                    },
                })
            })
            .collect()
    }

    /// Number of registered agents.
    pub fn len(&self) -> usize {
        self.inner.agents.len()
    }

    /// Whether the registry has no agents.
    pub fn is_empty(&self) -> bool {
        self.inner.agents.is_empty()
    }
}

/// Subset of an agent's metadata surfaced to the browser via
/// `GET /v1/agents`. The full JSON schema is intentionally not sent:
/// it is large, model-specific, and would needlessly leak LLM internals
/// to the UI layer.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentSummary {
    pub name: String,
    pub description: String,
}

#[cfg(feature = "datetime-agent")]
pub mod datetime_agent;
#[cfg(feature = "stock-agent")]
pub mod stock_agent;
#[cfg(feature = "weather-agent")]
pub mod weather_agent;
#[cfg(feature = "web-agent")]
pub mod web_fetch;
