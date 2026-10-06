//! Server-side chat agents (plan 4.C thin layer).
//!
//! The trait, the error type, the `UserContext`, the
//! `AgentRegistry`, and every agent implementation live in the
//! `nagent-agents` crate. This module is a thin server-side glue
//! layer:
//!
//! - Re-exports the trait + the registry so the rest of the server
//!   can keep `use crate::agents::{Agent, AgentRegistry}` without
//!   caring where they live.
//! - Builds the server-specific [`SecretSource`] impl that wraps
//!   [`crate::credentials::resolver::CredentialResolver`].
//! - Builds the server-specific [`DocumentSource`] impl that wraps
//!   [`crate::documents::DocumentStore`] + the chat-session
//!   binding, so the `read_document` agent can stay in
//!   `nagent-agents` without reaching into the DB.
//! - Provides the [`AgentRegistryFactory`] helper that the boot
//!   path calls to wire the registry given a `Config` +
//!   `DocumentStore` + `ChatSessions`. The static factory table
//!   lives on `nagent-agents::AgentRegistry::from_config`; this
//!   module wraps it with the server-only `read_document`
//!   registration.

pub mod routes;

use std::sync::Arc;

// Re-export every public item from the `nagent-agents` crate so the
// rest of the server code can keep referring to `crate::agents::Agent`,
// `crate::agents::UserContext`, etc.
pub use nagent_agents::{
    Agent, AgentError, AgentRegistry, AgentSummary, ConfirmationDecision, DocumentPayload,
    DocumentSource, SecretSource, UserContext,
};

/// Local newtype around [`AgentRegistry`] so we can implement
/// `FromRef<Arc<AppState>>` for it without tripping the orphan rule
/// (both the trait and the registry are now foreign types).
/// Handlers either destructure via the newtype's `Deref` impl or
/// use the blanket `impl From<AgentRegistryNewtype> for AgentRegistry`
/// in `state.rs`.
#[derive(Clone)]
pub struct AgentRegistryNewtype(pub AgentRegistry);

impl std::ops::Deref for AgentRegistryNewtype {
    type Target = AgentRegistry;
    fn deref(&self) -> &AgentRegistry {
        &self.0
    }
}

impl std::fmt::Debug for AgentRegistryNewtype {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("AgentRegistryNewtype")
            .field(&self.0)
            .finish()
    }
}

use crate::config::AgentConfig;
use crate::credentials::resolver::{CredentialError, CredentialResolver};
use crate::documents::DocumentStore;

/// Adapter that implements [`SecretSource`] on top of the server's
/// [`CredentialResolver`]. One per process; `Arc`-cloned into every
/// [`UserContext`].
pub struct ResolverSecretSource {
    resolver: Arc<CredentialResolver>,
}

impl std::fmt::Debug for ResolverSecretSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolverSecretSource")
            .field("resolver", &"<CredentialResolver>")
            .finish()
    }
}

impl ResolverSecretSource {
    pub fn new(resolver: Arc<CredentialResolver>) -> Self {
        Self { resolver }
    }
}

#[async_trait::async_trait]
impl SecretSource for ResolverSecretSource {
    async fn fetch(
        &self,
        user_id: uuid::Uuid,
        service: &str,
        field: &str,
    ) -> Result<Option<secrecy::SecretString>, AgentError> {
        // We deliberately do NOT use a per-request cache here —
        // `UserContext` has its own cache, and stacking two caches
        // doubles the memory footprint for no benefit. The
        // resolver's own plaintext buffer is wiped by
        // `SecretString::Drop`.
        match self.resolver.get_raw(user_id, service, field).await {
            Ok(opt) => Ok(opt),
            Err(CredentialError::Missing { service, field }) => {
                Err(AgentError::CredentialsMissing { service, field })
            }
            Err(CredentialError::DecryptFailed { service, field }) => {
                Err(AgentError::CredentialsDecryptFailed { service, field })
            }
            Err(CredentialError::Store(e)) => {
                Err(AgentError::AgentFailed(format!("credentials store: {e}")))
            }
        }
    }
}

/// Adapter that implements [`DocumentSource`] on top of the
/// server's [`DocumentStore`]. Constructed once per process and
/// cloned into the `ReadDocumentAgent` via `Arc`.
pub struct StoreDocumentSource {
    store: DocumentStore,
    chat_sessions: Option<crate::chat::sessions::ChatSessions>,
}

impl std::fmt::Debug for StoreDocumentSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreDocumentSource")
            .field("store", &"<DocumentStore>")
            .field(
                "chat_sessions",
                &self.chat_sessions.as_ref().map(|_| "<ChatSessions>"),
            )
            .finish()
    }
}

impl StoreDocumentSource {
    pub fn new(
        store: DocumentStore,
        chat_sessions: Option<crate::chat::sessions::ChatSessions>,
    ) -> Self {
        Self {
            store,
            chat_sessions,
        }
    }
}

#[async_trait::async_trait]
impl DocumentSource for StoreDocumentSource {
    fn max_extracted_chars(&self) -> usize {
        self.store.max_extracted_chars()
    }

    async fn read(
        &self,
        user_id: uuid::Uuid,
        chat_session_id: uuid::Uuid,
        name: &str,
    ) -> Result<DocumentPayload, AgentError> {
        // SEV 2 fix: verify the (user, session) binding minted by
        // POST /v1/chat/session before any DB lookup. A session id
        // that was minted by another user, or never minted at all,
        // surfaces a clear "forbidden" error so the LLM can
        // recover (and so the audit log records the attempt).
        // `touch_and_verify` ALSO refreshes `last_seen_at` so the
        // next periodic sweep sees a recent binding.
        if let Some(cs) = self.chat_sessions.as_ref() {
            cs.touch_and_verify(chat_session_id, user_id)
                .await
                .map_err(|_| {
                    AgentError::AgentFailed(
                        "chat session is not bound to the current user; \
                         ask the user to refresh the page"
                            .to_string(),
                    )
                })?;
        }
        // Fetch the document row, scoped to the user AND the
        // session. SEV 2 fix: a user cannot reach another user's
        // docs even if they guess the UUID.
        let row = self
            .store
            .db()
            .admin()
            .documents
            .get_by_name(name, user_id, chat_session_id)
            .await
            .map_err(|e| AgentError::AgentFailed(format!("document lookup failed: {e}")))?
            .ok_or_else(|| {
                AgentError::InvalidArguments(format!(
                    "unknown document `{name}` in this chat session"
                ))
            })?;
        // Read the file from disk. The DB row's `disk_path` is
        // operator-supplied / attacker-controlled if auth is
        // bypassed; `safe_disk_read` re-canonicalises the path and
        // refuses anything outside the cache dir.
        let cache_dir = self.store.cache_dir();
        let bytes = match crate::documents::storage::safe_disk_read(&row.disk_path, cache_dir) {
            Ok(b) => b,
            Err(crate::documents::storage::DiskReadError::EscapesCacheDir(_)) => {
                tracing::warn!(
                    document_id = %row.id,
                    path = %row.disk_path.display(),
                    "read_document: refusing to read disk_path outside cache_dir",
                );
                return Err(AgentError::AgentFailed(
                    "document file no longer available on disk; ask the user to re-upload".into(),
                ));
            }
            Err(crate::documents::storage::DiskReadError::Io(e))
                if e.kind() == std::io::ErrorKind::NotFound =>
            {
                return Err(AgentError::AgentFailed(
                    "document file no longer available on disk; ask the user to re-upload".into(),
                ));
            }
            Err(e) => {
                return Err(AgentError::AgentFailed(format!(
                    "could not read document: {e}"
                )));
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => {
                return Err(AgentError::AgentFailed(
                    "document is not valid UTF-8 (binary uploads are not supported)".into(),
                ));
            }
        };
        Ok(DocumentPayload {
            id: row.id,
            original_name: row.original_name,
            mime: row.mime,
            size_bytes: row.size_bytes,
            page_count: row.page_count,
            extracted_chars: row.extracted_chars,
            text,
        })
    }
}

/// Build the [`AgentRegistry`] from a server-side [`AgentConfig`] +
/// optional document store + chat-session binding.
///
/// When the documents subsystem is enabled AND the caller hands in
/// a non-`None` `DocumentStore`, the `read_document` agent is added
/// to the registry (gated by the `read-document-agent` cargo
/// feature).
pub fn build_registry(
    cfg: &AgentConfig,
    document_store: Option<DocumentStore>,
    chat_sessions: Option<crate::chat::sessions::ChatSessions>,
) -> AgentRegistry {
    // Plan 4.C: the plain per-agent *Config structs moved to
    // `nagent-agents`; the server-side `AgentConfig` owns the
    // `from_env_with_toml` parser and feeds the sub-configs to
    // `AgentRegistry::from_config` after a straight field-by-field
    // conversion (the runtime structs are deliberately identical
    // in shape so the conversion is a plain `clone()`).
    let cfgs = nagent_agents::AgentConfigs {
        web_fetch: cfg.web_fetch.clone().into(),
        weather: cfg.weather.clone().into(),
        unit_convert: cfg.unit_convert.clone().into(),
        wikipedia: cfg.wikipedia.clone().into(),
        dictionary: cfg.dictionary.clone().into(),
        stock: cfg.stock.clone().into(),
        calculate: Default::default(),
        datetime: Default::default(),
        read_document: cfg.read_document.clone().into(),
        caldav: cfg.caldav.clone().into(),
        x_timeline: cfg.x_timeline.clone().into(),
        memory: cfg.memory.clone().into(),
        // Plan 1791317253718: the `search_tools` meta-agent has
        // its own per-feature knob (`default_top_k`); the v1
        // server-side config does not surface it — `default()` is
        // the conservative starting point. A future operator knob
        // can be threaded through `AgentConfig` the same way the
        // other per-agent configs are.
        tool_search: Default::default(),
    };
    // Plan 4.C (C): one shared `EgressPool` is built per process
    // and passed to every agent. Strict-class agents
    // (`web_fetch`) drain the strict client; public-class agents
    // (`weather`, `dictionary`, `stock`, `wikipedia`) drain the
    // public client. Connection reuse + warm DNS / TLS roots are
    // the gains; per-agent SSRF policy still applies.
    let pool = nagent_agents::egress::EgressPool::new();
    let mut registry = AgentRegistry::from_config(&cfgs, cfg.enabled, &pool);
    if let (true, Some(store), true) = (cfg.enabled, document_store, cfg.read_document_enabled) {
        #[cfg(feature = "read-document-agent")]
        registry.push_agent_boxed(Box::new(nagent_agents::ReadDocumentAgent::new(Arc::new(
            StoreDocumentSource::new(store, chat_sessions),
        ))));
        // Without the `read-document-agent` cargo feature the
        // agent is not compiled in, so the request is a no-op.
        #[cfg(not(feature = "read-document-agent"))]
        let _ = (store, chat_sessions);
    }
    // Startup surface tool inventory. Logged once per registry build
    // so the operator can sanity-check what the registry is
    // shipping. The full list is no longer projected into
    // `tools=[]` on every round (plan 1791317253718 — the
    // round-level builder now ships only `search_tools` plus
    // the BM25 pre-selection plus the per-session discovered
    // set); the count + names are still useful for spotting
    // regressions like the CalDAV / `read_document` omissions of
    // earlier sessions, so the log line stays.
    let tool_names: Vec<String> = registry.iter().map(|a| a.name().to_string()).collect();
    if cfg.enabled {
        tracing::info!(
            count = tool_names.len(),
            tools = ?tool_names,
            "agents: tools registered (exposed to LLM via search_tools + BM25 pre-selection per round)"
        );
    } else {
        tracing::info!(
            "agents: tools disabled by config; proxy will send no tools=[] payload to the upstream LLM"
        );
    }
    registry
}

// ---- Server-side *Config → agents crate *Config adapters ------------------
//
// The server-side runtime structs are deliberately identical in shape
// to the agents crate's plain structs; the `From` impls below let
// `build_registry` convert the lot with a single `.into()` per field.
// When the shape diverges (e.g. a new knob the agents crate does
// not need), the `From` impl picks the safe default.

impl From<crate::config::WebFetchConfig> for nagent_agents::WebFetchAgentConfig {
    fn from(c: crate::config::WebFetchConfig) -> Self {
        Self {
            allow_public: c.allow_public,
            allowlist: c.allowlist,
            max_bytes: c.max_bytes,
            timeout_ms: c.timeout_ms,
        }
    }
}

impl From<crate::config::WeatherConfig> for nagent_agents::WeatherAgentConfig {
    fn from(c: crate::config::WeatherConfig) -> Self {
        Self {
            api_key: c.api_key,
            timeout_ms: c.timeout_ms,
            base_url: c.base_url,
        }
    }
}

impl From<crate::config::UnitConvertConfig> for nagent_agents::UnitConvertAgentConfig {
    fn from(c: crate::config::UnitConvertConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
        }
    }
}

impl From<crate::config::WikipediaConfig> for nagent_agents::WikipediaAgentConfig {
    fn from(c: crate::config::WikipediaConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
            base_url: c.base_url,
            user_agent: c.user_agent,
        }
    }
}

impl From<crate::config::DictionaryConfig> for nagent_agents::DictionaryAgentConfig {
    fn from(c: crate::config::DictionaryConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
            base_url: c.base_url,
        }
    }
}

impl From<crate::config::StockConfig> for nagent_agents::StockAgentConfig {
    fn from(c: crate::config::StockConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
        }
    }
}

impl From<crate::config::ReadDocumentConfig> for nagent_agents::ReadDocumentAgentConfig {
    fn from(c: crate::config::ReadDocumentConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
            max_extracted_chars: c.max_extracted_chars,
        }
    }
}

impl From<crate::config::agents::CalDavConfig> for nagent_agents::CalDavAgentConfig {
    fn from(c: crate::config::agents::CalDavConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
            allowlist: c.allowlist,
            max_events: c.max_events,
            max_body_bytes: c.max_body_bytes,
        }
    }
}

impl From<crate::config::agents::XTimelineConfig> for nagent_agents::XTimelineAgentConfig {
    fn from(c: crate::config::agents::XTimelineConfig) -> Self {
        Self {
            timeout_ms: c.timeout_ms,
            max_posts: c.max_posts,
            allowlist: c.allowlist,
            cache_ttl_secs: c.cache_ttl_secs,
            base_url: c.base_url,
        }
    }
}

impl From<crate::config::agents::MemoryConfig> for nagent_agents::config::MemoryAgentConfig {
    fn from(c: crate::config::agents::MemoryConfig) -> Self {
        // The runtime config is field-for-field identical to the
        // agents-crate struct; the only reason for the pair is
        // that the TOML / env parsing lives in `nagent-server`'s
        // `config::agents` module while the agents crate stays
        // TOML-free.
        Self {
            recalled_top_k: c.recalled_top_k,
        }
    }
}
