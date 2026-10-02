//! `Agent` trait, `UserContext`, `AgentRegistry`, and the agent
//! implementations.
//!
//! Plan 4.C: the entire agent subsystem lives here so adding a new
//! agent is "one directory, one factory line, one feature line,
//! docs". The crate has zero dependency on `nagent-server` or
//! `sqlx`; per-user credentials flow through the [`SecretSource`]
//! trait and per-session document access flows through the
//! [`DocumentSource`] trait. Both are implemented in `nagent-server`
//! (the resolver + the document store) and threaded into
//! [`UserContext::new`] at the request boundary.
//!
//! ## Agent metadata for the LLM layer
//!
//! The LLM tool loop applies the "may this tool run without an
//! extra user confirmation?" rule generically via
//! [`Agent::requires_confirmation`]; it does not know about
//! `web_fetch` or any other agent by name. A new agent that wants
//! to participate in the rule declares it once in its
//! implementation.
//!
//! The "is this agent's output safe to surface to the user
//! verbatim?" rule (used by the indirect prompt-injection block in
//! `llm/tool_loop.rs`) lives at [`Agent::untrusted_output`]; the
//! default is `true` because every agent ingests remote content.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::services::{ServiceDef, ServiceRegistry};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Capability traits
// ---------------------------------------------------------------------------

/// What an agent needs from the auth subtree to read a per-user
/// secret. The crate boundary must not let agents see the raw DB
/// pool or the AES-GCM key; only the resolver implements this and
/// exposes the per-call `user_id` + `(service, field) → plaintext`
/// projection.
#[async_trait]
pub trait SecretSource: Send + Sync {
    /// Look up a single credential field for `user_id`. Returns
    /// `Ok(None)` when the field has not been configured; returns
    /// `Err` for missing-resolver (test contexts), decrypt failure,
    /// or DB error. The plain [`SecretString`] wrapper zeroises the
    /// backing buffer when dropped.
    async fn fetch(
        &self,
        user_id: Uuid,
        service: &str,
        field: &str,
    ) -> Result<Option<secrecy::SecretString>, AgentError>;
}

/// What an agent needs from the documents subtree to fetch a single
/// uploaded document. The crate boundary must not let agents see
/// the cache directory layout; only the document store implements
/// this and exposes the read-by-name projection.
#[async_trait]
pub trait DocumentSource: Send + Sync {
    /// Fetch a document by `(user_id, chat_session_id, name)`.
    /// Returns `Err(AgentError::AgentFailed)` when the document is
    /// missing or not yet extracted; the agent surfaces the error
    /// verbatim to the LLM.
    async fn read(
        &self,
        user_id: Uuid,
        chat_session_id: Uuid,
        name: &str,
    ) -> Result<DocumentPayload, AgentError>;

    /// Truncation cap the agent applies to the returned text. The
    /// server reads its `[documents].max_extracted_chars` knob and
    /// surfaces it through this method so the agent does not have
    /// to know about the server config surface.
    fn max_extracted_chars(&self) -> usize;
}

/// Document payload returned by [`DocumentSource::read`]. Mirrors
/// the columns the `nagent_db::DocumentRow` already exposes, plus
/// the extracted text the agent returns verbatim to the LLM.
///
/// Lives at the `agents` module (not under `read_document`) so the
/// `DocumentSource` trait can reference the type unconditionally —
/// the trait is compiled even when the `read-document-agent`
/// cargo feature is off.
#[derive(Debug, Clone)]
pub struct DocumentPayload {
    pub id: Uuid,
    pub original_name: String,
    pub mime: String,
    pub size_bytes: u64,
    pub page_count: Option<u32>,
    pub extracted_chars: u64,
    pub text: String,
}

// ---------------------------------------------------------------------------
// Confirmation metadata
// ---------------------------------------------------------------------------

/// Outcome of [`Agent::requires_confirmation`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationDecision {
    /// The call is allowed without an extra confirmation step.
    Allow,
    /// The call needs an explicit user confirmation before it can
    /// proceed. The message is surfaced to the LLM verbatim as the
    /// `role: "tool"` content; the LLM is expected to ask the user
    /// in plain text and the next round will see `Allow`.
    NeedsConfirmation { reason: String },
}

impl ConfirmationDecision {
    /// `true` when the call is allowed without an extra step.
    pub fn is_allowed(&self) -> bool {
        matches!(self, ConfirmationDecision::Allow)
    }
}

// ---------------------------------------------------------------------------
// Agent trait
// ---------------------------------------------------------------------------

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
    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError>;

    /// Per-call policy: should the LLM tool loop refuse to dispatch
    /// this call without an extra user confirmation step? Default
    /// is `Allow` for every agent; `web_fetch` overrides this to
    /// enforce the indirect prompt-injection rule (plan #10).
    ///
    /// `history` is the set of agent names invoked earlier in the
    /// same chat-completions turn — agents that depend on
    /// order-of-invocation (e.g. `web_fetch` after `read_document`)
    /// inspect it via [`UserContext::invoked_this_turn`].
    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        ConfirmationDecision::Allow
    }

    /// Whether this agent's output is untrusted (default `true`).
    /// The LLM tool loop uses this to decide whether the agent's
    /// returned JSON should be wrapped in an untrusted-input fence
    /// before being sent back to the model. Agents that produce
    /// structured, deterministic output (date math, unit conversion)
    /// override to `false`.
    fn untrusted_output(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// UserContext
// ---------------------------------------------------------------------------

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
    /// `None` for `for_tests()` contexts (no resolver wired) and
    /// for any agent path that has not been wired with a resolver.
    /// When `None`, [`UserContext::secret`] returns
    /// `CredentialsMissing` without touching the DB.
    resolver: Option<Arc<dyn SecretSource>>,
    cache: crate::agents::credential_cache::SecretCache,
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
    /// Per-turn list of agent names invoked earlier. Cleared at the
    /// start of every chat-completions turn by the LLM tool loop;
    /// `web_fetch`'s `requires_confirmation` impl uses it to enforce
    /// the indirect prompt-injection rule.
    invoked_this_turn: Vec<String>,
}

impl std::fmt::Debug for UserContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserContext")
            .field("user_id", &self.user_id)
            .field("services", &self.services)
            .field("resolver", &self.resolver.as_ref().map(|_| "<resolver>"))
            .field("cache_entries", &self.cache.len())
            .field("chat_session_id", &self.chat_session_id)
            .field("invoked_this_turn", &self.invoked_this_turn)
            .finish()
    }
}

impl UserContext {
    /// Build a fresh context for one request. The caller owns the
    /// `ServiceRegistry` and `SecretSource` for the lifetime of the
    /// process (both live in `AppState`); we clone the `Arc`s into
    /// the per-request ctx.
    pub fn new(
        user_id: Uuid,
        services: Arc<ServiceRegistry>,
        resolver: Arc<dyn SecretSource>,
    ) -> Self {
        Self {
            user_id,
            services,
            resolver: Some(resolver),
            cache: crate::agents::credential_cache::SecretCache::new(),
            chat_session_id: None,
            invoked_this_turn: Vec::new(),
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
            cache: crate::agents::credential_cache::SecretCache::new(),
            chat_session_id: None,
            invoked_this_turn: Vec::new(),
        }
    }

    /// Constructor used by the LLM tool loop for one chat-completion
    /// request. `chat_session_id` is propagated from the browser's
    /// `X-Chat-Session-Id` header so per-session agents (currently
    /// `read_document`) can scope their queries.
    pub fn for_chat_session(
        user_id: Uuid,
        services: Arc<ServiceRegistry>,
        resolver: Option<Arc<dyn SecretSource>>,
        chat_session_id: Uuid,
    ) -> Self {
        Self {
            user_id,
            services,
            resolver,
            cache: crate::agents::credential_cache::SecretCache::new(),
            chat_session_id: Some(chat_session_id),
            invoked_this_turn: Vec::new(),
        }
    }

    /// Record that `name` was invoked in the current round. Called
    /// by the LLM tool loop right before each `Agent::invoke` so
    /// the agent's `requires_confirmation` impl can inspect the
    /// history.
    pub fn record_invocation(&mut self, name: &str) {
        if !self.invoked_this_turn.iter().any(|n| n == name) {
            self.invoked_this_turn.push(name.to_string());
        }
    }

    /// True when `name` was invoked earlier in the same
    /// chat-completions turn.
    pub fn was_invoked(&self, name: &str) -> bool {
        self.invoked_this_turn.iter().any(|n| n == name)
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
    ) -> Result<Option<secrecy::SecretString>, AgentError> {
        let Some(resolver) = &self.resolver else {
            return Err(AgentError::CredentialsMissing {
                service: service.to_string(),
                field: field.to_string(),
            });
        };
        // First consult the per-request cache (mirrors the
        // historical behaviour so agent code can make multiple
        // `secret()` calls in one invocation without a DB round
        // trip).
        if let Some(cached) = self.cache.get(service, field) {
            return Ok(Some(cached));
        }
        match resolver.fetch(self.user_id, service, field).await {
            Ok(Some(plaintext)) => {
                self.cache.insert(service, field, plaintext.clone());
                Ok(Some(plaintext))
            }
            Ok(None) => Err(AgentError::CredentialsMissing {
                service: service.to_string(),
                field: field.to_string(),
            }),
            Err(e @ AgentError::CredentialsDecryptFailed { .. }) => Err(e),
            Err(other) => Err(other),
        }
    }
}

impl Drop for UserContext {
    fn drop(&mut self) {
        // Zeroise the in-memory plaintext cache. The
        // `SecretString::Drop` impl already wipes each entry;
        // clearing the inner map drives the `Drop` for every entry.
        self.cache.zeroize();
    }
}

// ---------------------------------------------------------------------------
// AgentRegistry
// ---------------------------------------------------------------------------

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

impl AgentRegistryInner {
    fn push(&mut self, agent: Arc<dyn Agent>) {
        self.agents.push(agent);
    }
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

    /// Build a registry from the plain per-agent configs. The
    /// static [`AGENT_DESCRIPTORS`] table is walked per descriptor,
    /// each gated by its own cargo feature so a slim build drops
    /// the heavy machinery (plan 4.C: one feature per agent).
    ///
    /// `enabled` is the operator-controlled master switch; when
    /// `false` the function returns `Self::empty()` so the LLM
    /// proxy injects no `tools` field.
    ///
    /// A descriptor that returns `Err` from its `build` closure
    /// is logged at `warn` and skipped — the rest of the registry
    /// still loads. Today every descriptor is infallible; the
    /// branch exists so a future agent can validate its config
    /// without breaking the others.
    pub fn from_config(cfgs: &crate::config::AgentConfigs, enabled: bool) -> Self {
        if !enabled {
            return Self::empty();
        }
        let mut registry = Self::empty();
        for descriptor in AGENT_DESCRIPTORS {
            match (descriptor.build)(cfgs) {
                Ok(agent) => registry.push_agent_boxed(agent),
                Err(e) => {
                    tracing::warn!(
                        agent = descriptor.id,
                        feature = descriptor.feature,
                        error = %e,
                        "skipping agent descriptor: build failed"
                    );
                }
            }
        }
        registry
    }

    /// Push an additional agent that was constructed outside the
    /// static per-feature factory table (e.g. `read_document`, which
    /// needs a server-built `DocumentSource`). The boxed trait
    /// object is converted to an `Arc<dyn Agent>` and appended; if
    /// the registry was already shared (cloned) we rebuild from
    /// the current list so the mutation lands.
    pub fn push_agent_boxed(&mut self, agent: Box<dyn Agent>) {
        let agent: Arc<dyn Agent> = Arc::from(agent);
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.push(agent);
        } else {
            // Already shared — rebuild.
            let mut next = AgentRegistryInner {
                agents: self.inner.agents.clone(),
            };
            next.push(agent);
            self.inner = Arc::new(next);
        }
    }

    /// Convenience wrapper that boxes a concrete agent and appends
    /// it. Used by [`Self::from_config`] for every per-feature agent
    /// and by the server-side glue layer for the `read_document`
    /// agent.
    pub fn push_agent<A: Agent + 'static>(&mut self, agent: A) {
        self.push_agent_boxed(Box::new(agent));
    }

    /// Iterate every registered agent. Used by the test helpers and
    /// the (rare) /debug/agents endpoint.
    pub fn iter(&self) -> impl Iterator<Item = Arc<dyn Agent>> + '_ {
        self.inner.agents.iter().cloned()
    }

    /// Concise description used by `GET /v1/agents`. Schema details
    /// are intentionally omitted — the LLM sees the schema via the
    /// `tools` array, not the browser.
    pub fn list(&self) -> Vec<AgentSummary> {
        self.inner
            .agents
            .iter()
            .map(|a| {
                let dummy_ctx =
                    UserContext::for_tests(Uuid::nil(), ServiceRegistry::empty().into_arc());
                AgentSummary {
                    name: a.name().to_string(),
                    description: a.description().to_string(),
                    untrusted_output: a.untrusted_output(),
                    requires_confirmation_by_default: !a
                        .requires_confirmation(&dummy_ctx, &Value::Null)
                        .is_allowed(),
                }
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
#[derive(Debug, Clone, Serialize)]
pub struct AgentSummary {
    pub name: String,
    pub description: String,
    pub untrusted_output: bool,
    pub requires_confirmation_by_default: bool,
}

// ---------------------------------------------------------------------------
// Static agent descriptor table
// ---------------------------------------------------------------------------
//
// One line per agent (plan 4.C). Adding a new agent is:
//
// 1. create `crates/nagent-agents/src/agents/<name>.rs`,
// 2. add the agent's per-feature config to `crate::config::AgentConfigs`,
// 3. add the agent's `mod <name>;` declaration (cargo-feature gated)
//    to the sub-modules section below,
// 4. add one `AgentDescriptor` entry to [`AGENT_DESCRIPTORS`],
// 5. add the new agent's feature to the crate's `[features]` section
//    in `Cargo.toml` and to the `all-agents` meta-feature.
//
// The descriptor's `build` closure is invoked by
// [`AgentRegistry::from_config`] under the same `enabled`
// master switch as the historical hand-rolled `if cfg!(...)`
// chain, so a slim build that drops a feature compiles
// straight through the remaining descriptors.

/// Boxed error returned by an [`AgentDescriptor::build`] when the
/// agent's config is invalid. Lives as a type alias so the
/// signature of [`AgentDescriptor::build`] stays readable.
pub type AgentBuildError = Box<dyn std::error::Error + Send + Sync>;

/// Factory + identity for one agent. Constructed inline by
/// [`AGENT_DESCRIPTORS`] so the entry has no allocation cost.
pub struct AgentDescriptor {
    /// Agent id. Must match the `Agent::name()` of the
    /// constructed instance — the LLM tool loop uses it as the
    /// `function.name` in the OpenAI `tools` payload.
    pub id: &'static str,
    /// Cargo feature that must be on for this agent to compile.
    /// Surfaced for the `get_log` debug helper; not enforced
    /// at runtime because the table itself is `cfg`-gated.
    pub feature: &'static str,
    /// Build a boxed instance from the global agent configs.
    /// Returning `Err` is reserved for agents whose config is
    /// invalid (a missing API key, an unparseable base URL); the
    /// registry will skip the agent and log a warning. Today
    /// every entry is infallible; the `Result` exists so a
    /// future agent can fail gracefully without breaking the
    /// other descriptors.
    pub build: fn(&crate::config::AgentConfigs) -> Result<Box<dyn Agent>, AgentBuildError>,
}

/// Static factory table. Walked by
/// [`AgentRegistry::from_config`]; every entry is gated by its
/// own cargo feature so a slim build drops the heavy machinery
/// (plan 4.C: one feature per agent).
pub static AGENT_DESCRIPTORS: &[AgentDescriptor] = &[
    #[cfg(feature = "web-agent")]
    AgentDescriptor {
        id: "web_fetch",
        feature: "web-agent",
        build: |cfgs| {
            Ok(Box::new(web_fetch::WebFetchAgent::new(
                cfgs.web_fetch.clone(),
            )))
        },
    },
    #[cfg(feature = "datetime-agent")]
    AgentDescriptor {
        id: "get_datetime",
        feature: "datetime-agent",
        build: |cfgs| {
            Ok(Box::new(datetime_agent::DateTimeAgent::from_config(
                cfgs.datetime.clone(),
            )))
        },
    },
    #[cfg(feature = "weather-agent")]
    AgentDescriptor {
        id: "get_weather",
        feature: "weather-agent",
        build: |cfgs| {
            Ok(Box::new(weather_agent::WeatherAgent::new(
                cfgs.weather.clone(),
            )))
        },
    },
    #[cfg(feature = "stock-agent")]
    AgentDescriptor {
        id: "get_stock_quote",
        feature: "stock-agent",
        build: |cfgs| {
            Ok(Box::new(stock_agent::StockAgent::from_config(
                cfgs.stock.clone(),
            )))
        },
    },
    #[cfg(feature = "calculate-agent")]
    AgentDescriptor {
        id: "calculate",
        feature: "calculate-agent",
        build: |cfgs| {
            Ok(Box::new(calculate_agent::CalculateAgent::from_config(
                cfgs.calculate.clone(),
            )))
        },
    },
    #[cfg(feature = "unit-convert-agent")]
    AgentDescriptor {
        id: "unit_convert",
        feature: "unit-convert-agent",
        build: |cfgs| {
            Ok(Box::new(unit_convert_agent::UnitConvertAgent::new(
                cfgs.unit_convert.clone(),
            )))
        },
    },
    #[cfg(feature = "wikipedia-agent")]
    AgentDescriptor {
        id: "wikipedia",
        feature: "wikipedia-agent",
        build: |cfgs| {
            Ok(Box::new(wikipedia_agent::WikipediaAgent::new(
                cfgs.wikipedia.clone(),
            )))
        },
    },
    #[cfg(feature = "dictionary-agent")]
    AgentDescriptor {
        id: "dictionary",
        feature: "dictionary-agent",
        build: |cfgs| {
            Ok(Box::new(dictionary_agent::DictionaryAgent::new(
                cfgs.dictionary.clone(),
            )))
        },
    },
];

#[cfg(test)]
mod descriptor_table_tests {
    use super::*;
    use crate::config::AgentConfigs;

    #[test]
    fn every_descriptor_id_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for d in AGENT_DESCRIPTORS {
            assert!(seen.insert(d.id), "duplicate AgentDescriptor id: {}", d.id);
            assert!(!d.id.is_empty(), "AgentDescriptor id must be non-empty");
            assert!(
                !d.feature.is_empty(),
                "AgentDescriptor feature must be non-empty"
            );
        }
    }

    #[test]
    fn every_descriptor_builds_an_agent_with_matching_id() {
        // End-to-end check: every descriptor in the table
        // produces an agent whose `name()` matches the
        // descriptor's `id`. Guards against a future
        // copy-paste typo in either field.
        let cfgs = AgentConfigs::default();
        for d in AGENT_DESCRIPTORS {
            let agent = (d.build)(&cfgs)
                .unwrap_or_else(|e| panic!("descriptor {} build() failed: {e}", d.id));
            assert_eq!(
                agent.name(),
                d.id,
                "descriptor {} built an agent with name() = {}",
                d.id,
                agent.name()
            );
        }
    }

    #[test]
    fn descriptor_table_is_nonempty_when_any_feature_is_on() {
        // The `all-agents` meta-feature turns every per-agent
        // feature on. The CI default build runs with
        // --features all-agents,test/,so AGENT_DESCRIPTORS must
        // be non-empty there. If the meta-feature is dropped
        // (e.g. a contributor built only `--features agent` for
        // a faster check), the assertion is best-effort and
        // stays harmless.
        #[cfg(all(
            feature = "all-agents",
            any(
                feature = "web-agent",
                feature = "datetime-agent",
                feature = "weather-agent",
                feature = "stock-agent",
                feature = "calculate-agent",
                feature = "unit-convert-agent",
                feature = "wikipedia-agent",
                feature = "dictionary-agent",
            )
        ))]
        assert!(
            !AGENT_DESCRIPTORS.is_empty(),
            "AGENT_DESCRIPTORS must list at least one agent when all-agents is on"
        );
    }
}

// ---------------------------------------------------------------------------
// Sub-modules
// ---------------------------------------------------------------------------

/// Per-request plaintext credential cache. Lives inside
/// [`UserContext`]; same shape as the historical server-side cache.
pub mod credential_cache;

/// Plain per-agent *Config structs (defaults only).
pub mod config_doc;
pub mod web_fetch;

#[cfg(feature = "calculate-agent")]
pub mod calculate_agent;
#[cfg(feature = "datetime-agent")]
pub mod datetime_agent;
#[cfg(feature = "dictionary-agent")]
pub mod dictionary_agent;
#[cfg(feature = "read-document-agent")]
pub mod read_document;
#[cfg(feature = "stock-agent")]
pub mod stock_agent;
#[cfg(feature = "unit-convert-agent")]
pub mod unit_convert_agent;
#[cfg(feature = "weather-agent")]
pub mod weather_agent;
#[cfg(feature = "wikipedia-agent")]
pub mod wikipedia_agent;
