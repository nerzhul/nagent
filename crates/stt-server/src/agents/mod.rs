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
//!
//! ## Per-user credentials
//!
//! Every [`Agent::invoke`] receives a `&UserContext`. Agents that
//! need to talk to a third-party service on the user's behalf call
//! [`UserContext::secret`] which decrypts a per-(user, service,
//! field) ciphertext on demand, caches the plaintext for the rest of
//! the request, and writes one `auth_events` audit row. Agents that
//! do not need credentials ignore the argument.

pub mod routes;
pub mod services;

use std::sync::Arc;

use async_trait::async_trait;
use secrecy::SecretString;
use serde_json::Value;
use uuid::Uuid;

use crate::credentials::cache::SecretCache;
use crate::credentials::resolver::{CredentialError, CredentialResolver};

pub use services::{
    FieldDef, FieldKind, FieldSummary, ServiceDef, ServiceRegistry, ServiceSummary,
};

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
    /// The agent tried to read a per-user credential but the user
    /// has not configured that (service, field) yet. Surfaced as a
    /// 4xx-class signal to the LLM so the chat UI can prompt the
    /// user to configure the integration; never logged at WARN.
    #[error("credentials not configured for service={service} field={field}")]
    CredentialsMissing { service: String, field: String },
    /// The persisted ciphertext for a credential failed AES-GCM
    /// authentication. Almost always means
    /// `[auth.credentials].key` was rotated without re-encrypting
    /// the rows; the row is now unrecoverable.
    #[error("credentials decrypt failed for service={service} field={field}")]
    CredentialsDecryptFailed { service: String, field: String },
}

impl From<CredentialError> for AgentError {
    fn from(e: CredentialError) -> Self {
        match e {
            CredentialError::Missing { service, field } => {
                AgentError::CredentialsMissing { service, field }
            }
            CredentialError::DecryptFailed { service, field } => {
                AgentError::CredentialsDecryptFailed { service, field }
            }
            CredentialError::Store(e) => {
                AgentError::AgentFailed(format!("credentials store error: {e}"))
            }
        }
    }
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
    ///
    /// `ctx` carries the calling user's identity and the per-request
    /// plaintext cache. Agents that do not need credentials may
    /// ignore it (let-binding `_ctx: &UserContext`).
    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError>;
}

/// Per-call context handed to every [`Agent::invoke`]. Cheap to
/// construct (the `SecretCache` allocates one DashMap); the `Drop`
/// impl zeroises the cache so plaintexts never outlive the request.
///
/// Constructed once per LLM tool round (or once per direct
/// `/v1/agents/:name/invoke` call) so each call has its own cache —
/// no long-lived plaintext cache anywhere in the process.
pub struct UserContext {
    user_id: Uuid,
    services: Arc<ServiceRegistry>,
    /// `None` for `for_tests()` contexts (no DB available) and for
    /// any agent path that has not been wired with a resolver.
    /// When `None`, [`UserContext::secret`] returns
    /// `CredentialsMissing` without touching the DB.
    resolver: Option<Arc<CredentialResolver>>,
    cache: SecretCache,
    /// Active chat session id, when the agent was invoked from the
    /// `/v1/chat/completions` tool loop. The id is propagated from
    /// the browser's `X-Chat-Session-Id` header; agents that are
    /// scoped to a single conversation (currently `read_document`)
    /// read it via [`UserContext::chat_session_id`].
    ///
    /// `None` for direct `/v1/agents/:name/invoke` calls and for
    /// test contexts; the `read_document` agent panics with a clear
    /// error when this is `None`.
    chat_session_id: Option<Uuid>,
}

impl std::fmt::Debug for UserContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserContext")
            .field("user_id", &self.user_id)
            .field("services", &self.services)
            .field("resolver", &self.resolver.as_ref().map(|_| "<resolver>"))
            .field("cache_entries", &self.cache.len())
            .field("chat_session_id", &self.chat_session_id)
            .finish()
    }
}

impl UserContext {
    /// Build a fresh context for one request. The caller owns the
    /// `ServiceRegistry` and `CredentialResolver` for the lifetime
    /// of the process (both live in `AppState`); we clone the
    /// `Arc`s into the per-request ctx.
    pub fn new(
        user_id: Uuid,
        services: Arc<ServiceRegistry>,
        resolver: Arc<CredentialResolver>,
    ) -> Self {
        Self {
            user_id,
            services,
            resolver: Some(resolver),
            cache: SecretCache::new(),
            chat_session_id: None,
        }
    }

    /// Test-only constructor that omits the credential resolver.
    ///
    /// Existing agent unit tests that ignore `ctx` (the vast
    /// majority) call this and keep working without a database.
    /// Tests that exercise `ctx.secret(...)` must use the real
    /// [`UserContext::new`] with a wired resolver.
    pub fn for_tests(user_id: Uuid, services: Arc<ServiceRegistry>) -> Self {
        Self {
            user_id,
            services,
            resolver: None,
            cache: SecretCache::new(),
            chat_session_id: None,
        }
    }

    /// Constructor used by the LLM tool loop for one chat-completion
    /// request. `chat_session_id` is propagated from the browser's
    /// `X-Chat-Session-Id` header so per-session agents (currently
    /// `read_document`) can scope their queries.
    ///
    /// The optional resolver mirrors `new` / `for_tests` semantics:
    /// direct curl callers (no auth) get a `None` here, the
    /// chat-completions path gets a `Some` whenever the auth
    /// subsystem is enabled.
    pub fn for_chat_session(
        user_id: Uuid,
        services: Arc<ServiceRegistry>,
        resolver: Option<Arc<CredentialResolver>>,
        chat_session_id: Uuid,
    ) -> Self {
        Self {
            user_id,
            services,
            resolver,
            cache: SecretCache::new(),
            chat_session_id: Some(chat_session_id),
        }
    }

    /// Active chat session id when the agent was invoked from the
    /// LLM tool loop. `None` for direct `/v1/agents/:name/invoke`
    /// calls (curl, integration tests) and for test contexts.
    pub fn chat_session_id(&self) -> Option<Uuid> {
        self.chat_session_id
    }

    /// The calling user's UUID. Agents can use it for audit rows
    /// outside the credential path (the credential resolver
    /// already records its own rows).
    pub fn user_id(&self) -> Uuid {
        self.user_id
    }

    /// Look up a service by id.
    pub fn service_def(&self, id: &str) -> Option<&'static ServiceDef> {
        self.services.get(id)
    }

    /// Iterate every registered service (used by the LLM prompt to
    /// emit the configured-only integrations block).
    pub fn services(&self) -> &'static [ServiceDef] {
        self.services.list()
    }

    /// Resolve a single credential field for the calling user.
    ///
    /// Returns:
    /// - `Ok(Some(plaintext))` on hit (subsequent calls within the
    ///   same request hit the cache);
    /// - `Err(AgentError::CredentialsMissing)` when the field has
    ///   not been configured OR when no resolver is wired (test
    ///   contexts);
    /// - `Err(AgentError::CredentialsDecryptFailed)` when AES-GCM
    ///   authentication fails.
    pub async fn secret(
        &self,
        service: &str,
        field: &str,
    ) -> Result<Option<SecretString>, AgentError> {
        let Some(resolver) = &self.resolver else {
            return Err(AgentError::CredentialsMissing {
                service: service.to_string(),
                field: field.to_string(),
            });
        };
        match resolver
            .get(self.user_id, service, field, &self.cache)
            .await
        {
            Ok(opt) => Ok(opt),
            Err(e) => Err(AgentError::from(e)),
        }
    }
}

impl Drop for UserContext {
    fn drop(&mut self) {
        // Zeroise the in-memory plaintext cache. The
        // `SecretString::Drop` impl already wipes each entry;
        // clearing the DashMap drives the `Drop` for every entry.
        self.cache.zeroize();
    }
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
            agents.push(Arc::new(weather_agent::WeatherAgent::new(
                cfg.weather.clone(),
            )));
        }
        #[cfg(feature = "stock-agent")]
        {
            agents.push(Arc::new(stock_agent::StockAgent::new()));
        }
        #[cfg(feature = "calculate-agent")]
        {
            agents.push(Arc::new(calculate_agent::CalculateAgent::new()));
        }
        #[cfg(feature = "unit-convert-agent")]
        {
            agents.push(Arc::new(unit_convert_agent::UnitConvertAgent::new(
                cfg.unit_convert.clone(),
            )));
        }
        #[cfg(feature = "wikipedia-agent")]
        {
            agents.push(Arc::new(wikipedia_agent::WikipediaAgent::new(
                cfg.wikipedia.clone(),
            )));
        }
        #[cfg(feature = "dictionary-agent")]
        {
            agents.push(Arc::new(dictionary_agent::DictionaryAgent::new(
                cfg.dictionary.clone(),
            )));
        }
        #[cfg(not(any(
            feature = "web-agent",
            feature = "datetime-agent",
            feature = "weather-agent",
            feature = "stock-agent",
            feature = "calculate-agent",
            feature = "unit-convert-agent",
            feature = "wikipedia-agent",
            feature = "dictionary-agent"
        )))]
        {
            let _ = cfg; // suppress unused warning when no agent features are on
        }
        Self {
            inner: Arc::new(AgentRegistryInner { agents }),
        }
    }

    /// Variant of [`AgentRegistry::from_config`] that also wires
    /// the `read_document` agent when the documents feature is
    /// runtime-enabled. `from_config` stays unchanged so existing
    /// call sites keep working unmodified.
    pub fn from_config_with_documents(
        cfg: &super::config::AgentConfig,
        documents: &crate::config::DocumentsConfig,
        document_store: Option<crate::documents::DocumentStore>,
        chat_sessions: Option<crate::chat_sessions::ChatSessions>,
    ) -> Self {
        let mut registry = Self::from_config(cfg);
        if !cfg.enabled {
            return registry;
        }
        if !documents.enabled {
            return registry;
        }
        if let Some(store) = document_store {
            // Re-wrap into the mutable representation so we can
            // append the new agent. The default `from_config`
            // returned a registry with an internal `Arc`; cloning
            // and re-wrapping would lose us the ability to
            // mutate, so we build a fresh registry from scratch
            // using the same set of agents as `from_config` PLUS
            // the document agent.
            let mut agents: Vec<Arc<dyn Agent>> = Vec::new();
            if let Some(inner) = Arc::get_mut(&mut registry.inner) {
                agents.extend(inner.agents.iter().cloned());
            } else {
                // First `Arc::get_mut` fails when the registry was
                // already cloned (e.g. handed off to AppState).
                // Rebuild from the public list.
                for a in registry.list() {
                    if let Some(arc) = registry.get(&a.name) {
                        agents.push(arc);
                    }
                }
            }
            agents.push(Arc::new(crate::documents::agent::ReadDocumentAgent::new(
                store,
                chat_sessions,
            )));
            registry.inner = Arc::new(AgentRegistryInner { agents });
        }
        registry
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

#[cfg(feature = "calculate-agent")]
pub mod calculate_agent;
#[cfg(feature = "datetime-agent")]
pub mod datetime_agent;
#[cfg(feature = "dictionary-agent")]
pub mod dictionary_agent;
#[cfg(feature = "stock-agent")]
pub mod stock_agent;
#[cfg(feature = "unit-convert-agent")]
pub mod unit_convert_agent;
#[cfg(feature = "weather-agent")]
pub mod weather_agent;
#[cfg(feature = "web-agent")]
pub mod web_fetch;
#[cfg(feature = "wikipedia-agent")]
pub mod wikipedia_agent;
