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

use crate::egress::EgressPool;
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

/// What an agent needs from the auth subtree to **write back**
/// refreshed per-user secrets (plan 1790695073418 — the X OAuth
/// "refresh handled by the agent itself" locked decision).
///
/// Mirror image of [`SecretSource`]: the same crate boundary is
/// enforced (agents cannot reach the DB / key), the impl lives in
/// `nagent-server` next to the `CredentialResolver`, and the
/// [`UserContext`] `sink` field is `None` outside the chat-session
/// constructor so test contexts and direct-invoke paths stay
/// sink-free.
#[async_trait]
pub trait SecretSink: Send + Sync {
    /// Replace the supplied `(service, field) → plaintext` pairs
    /// atomically for `user_id`. The implementation must
    /// encrypt + UPSERT every row and write one audit row of
    /// kind `credential_access` (same as a read on the resolver
    /// side) so the audit log reads uniformly across reads and
    /// writes.
    ///
    /// `fields` is `&[(&str, secrecy::SecretString)]` so callers
    /// can hand the write-back path the same `SecretString` they
    /// received from [`SecretSource::fetch`] without an extra
    /// clone.
    async fn update(
        &self,
        user_id: Uuid,
        service: &str,
        fields: &[(&str, secrecy::SecretString)],
    ) -> Result<(), AgentError>;
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

    /// Per-call page-range read entry point. Implementations
    /// decrypt the requested pages from the per-doc encrypted
    /// `<uuid>/pages/` directory and cap the response at
    /// `max_pages_per_call` / `max_page_chars_per_call`. The
    /// default implementation falls back to [`Self::read`] +
    /// truncate, which keeps the trait extensible for downstream
    /// `DocumentSource` impls that have not yet ported to the
    /// per-page store.
    ///
    /// Returning `Ok(DocumentPayload::Range { .. })` is the
    /// preferred response; the default delegates to
    /// [`Self::read`] and packages the full text as a single-page
    /// range so the existing wire shape still carries the
    /// "showing X-Y of N" hint.
    async fn read_with_request(
        &self,
        user_id: Uuid,
        chat_session_id: Uuid,
        request: DocumentReadRequest,
    ) -> Result<DocumentPayload, AgentError> {
        // Default: fetch the full payload via the legacy path
        // and let the agent slice / cap it. Downstream
        // implementers that do not have a per-page store can
        // keep relying on this; only the server-side store
        // overrides with the encrypted-blob path.
        let _ = request; // not used in the legacy path
        self.read(user_id, chat_session_id, &request.name).await
    }

    /// Hard cap on the number of pages one range-mode read may
    /// return. Configured by `[documents].max_pages_per_call`.
    fn max_pages_per_call(&self) -> u32;

    /// Hard cap on the total characters one range-mode read may
    /// return. Configured by `[documents].max_page_chars_per_call`.
    fn max_page_chars_per_call(&self) -> usize;
}

/// Parsed page range applied to a [`DocumentSource::read_with_request`]
/// call. Lives at the agents layer so the `DocumentSource` trait
/// can reference the type unconditionally — implementations that
/// have not migrated to the per-page store can simply ignore it
/// (the default `read_with_request` does exactly that).
///
/// Both bounds are 1-indexed to match the LLM-facing
/// `page_range` argument shape (`"3"`, `"3-7"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentReadRequest {
    pub name: String,
    pub page_range: Option<PageRange>,
}

/// Inclusive `[start, end]` 1-indexed page range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRange {
    pub start: u32,
    pub end_inclusive: u32,
}

/// Document payload returned by [`DocumentSource::read`] and
/// [`DocumentSource::read_with_request`].
///
/// Lives at the `agents` module (not under `read_document`) so the
/// `DocumentSource` trait can reference the type unconditionally —
/// the trait is compiled even when the `read-document-agent` cargo
/// feature is off.
///
/// Variants:
///
/// - `Overview` — full document summary returned when the LLM
///   calls `read_document(name)` with no `page_range`. Carries the
///   real `page_count`, a short `preview` (first ~2 000 chars),
///   and an optional table of contents. The LLM is told to make
///   follow-up calls with `page_range` to fetch specific pages.
/// - `Range` — the joined text of the requested pages plus the
///   applied range, capped at `max_pages_per_call` pages and
///   `max_page_chars_per_call` characters. The agent's response
///   envelope surfaces `pages X-Y of N` so the LLM knows how
///   much of the document is still unread.
/// - `FullText` — legacy mode used by the default
///   [`DocumentSource::read_with_request`] impl; the full document
///   text is returned verbatim (existing behaviour for
///   `.txt`/`.md`/`.log` files where pagination does not apply).
#[derive(Debug, Clone)]
pub struct DocumentPayload {
    pub id: Uuid,
    pub original_name: String,
    pub mime: String,
    pub size_bytes: u64,
    pub page_count: Option<u32>,
    pub extracted_chars: u64,
    /// Number of pages whose text extraction failed at upload
    /// time (e.g. an unparseable ToUnicode CMap). Always 0 for
    /// plain-text rows. Surfaced in the overview envelope so
    /// the LLM can tell when a document is entirely unreadable
    /// and the preview is therefore empty.
    pub unreadable_pages: u32,
    pub shape: DocumentShape,
}

impl DocumentPayload {
    /// Build a `FullText` payload with the legacy shape — the
    /// extracted text is returned verbatim. Used by the default
    /// [`DocumentSource::read`] path and by `.txt`/`.md`/`.log`
    /// rows that have no per-page store.
    pub fn full_text(
        id: Uuid,
        original_name: String,
        mime: String,
        size_bytes: u64,
        page_count: Option<u32>,
        extracted_chars: u64,
        text: String,
    ) -> Self {
        Self {
            id,
            original_name,
            mime,
            size_bytes,
            page_count,
            extracted_chars,
            unreadable_pages: 0,
            shape: DocumentShape::FullText(text),
        }
    }

    /// Re-shape the payload while keeping the metadata fields.
    /// The agent side picks `Overview` / `Range` based on whether
    /// the LLM requested a `page_range`; the document store hands
    /// back a `FullText` payload first and then narrows it down
    /// once the encryption round-trip has succeeded.
    pub fn with_shape(mut self, shape: DocumentShape) -> Self {
        self.shape = shape;
        self
    }

    /// Mark the payload as carrying N unreadable pages (PDF
    /// only). The agent's overview envelope surfaces this count
    /// so the LLM knows when a document is entirely
    /// unparseable.
    pub fn with_unreadable_pages(mut self, unreadable: u32) -> Self {
        self.unreadable_pages = unreadable;
        self
    }

    /// Borrow the returned text regardless of the underlying
    /// shape. Range and FullText both carry the joined text;
    /// Overview carries the preview text in the same field.
    pub fn joined_text(&self) -> &str {
        match &self.shape {
            DocumentShape::FullText(s) | DocumentShape::Range(s) | DocumentShape::Overview(s) => s,
        }
    }
}

/// Underlying shape of a [`DocumentPayload`]. See the type-level
/// docs on [`DocumentPayload`] for the per-variant contract.
#[derive(Debug, Clone)]
pub enum DocumentShape {
    /// Overview-only payload. `String` is the short preview
    /// (~2 000 chars). The agent's response envelope surfaces the
    /// page count and the table-of-contents alongside this text.
    Overview(String),
    /// Range payload. `String` is the joined pages with per-page
    /// separators (`--- page N ---`). The agent's response
    /// envelope surfaces the applied range so the LLM knows what
    /// it received.
    Range(String),
    /// Full-document payload. `String` is the entire extracted
    /// text. The agent's response envelope surfaces `truncated`
    /// when the truncation cap fires.
    FullText(String),
}

/// What an agent needs from the memory subtree to read / write /
/// list / forget per-user long-term memories. Plan 1791267136806.
///
/// The crate boundary is the same shape as the other capability
/// traits: the agents crate never sees the raw `nagent_db::Db` or
/// the AES-GCM key. The implementation lives in `nagent-server`
/// (`UserDbMemorySource`, which holds an `Arc<CredentialsKey>` and
/// wraps `nagent_db::memories::Memories`).
///
/// Recall returns `DecryptedMemory` so the agents receive the
/// plaintext `value` / `notes` inside a `SecretString`; the per-
/// request plaintext lifetime is bounded by the calling scope
/// (the `UserContext::Drop` impl zeroises the secret cache, and the
/// `DecryptedMemory` itself is held only for the duration of the
/// `Agent::invoke` call).
#[async_trait]
pub trait MemorySource: Send + Sync {
    /// Persist one (plaintext) memory for `user_id`. The
    /// implementation encrypts `value` + `notes` with the server-
    /// side `[auth.credentials].key` (reusing the existing AES-256-
    /// GCM helpers from `nagent_server::credentials::crypto`) and
    /// upserts the row through `nagent_db::memories::Memories`.
    /// Returns the new (or pre-existing, on `(subject, predicate)`
    /// collision) memory id.
    async fn store(&self, user_id: Uuid, request: MemoryWriteRequest) -> Result<Uuid, AgentError>;

    /// Decrypt + return up to `limit` memories for `user_id`,
    /// optionally filtered by `subject` / `predicate` / `tags`
    /// (case-insensitive `LIKE` patterns with `%` wildcards).
    /// `limit` is hard-capped at the repository level
    /// (`memories::RECALL_HARD_LIMIT = 64`) so a corrupt row cannot
    /// drive unbounded decrypt work.
    async fn recall(
        &self,
        user_id: Uuid,
        subject: Option<&str>,
        predicate: Option<&str>,
        tags: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DecryptedMemory>, AgentError>;

    /// List metadata for `user_id`, newest first. Metadata-only —
    /// the encrypted bytes are NOT returned (the SPA renders the
    /// Settings tab Memory list with `subject` / `predicate` /
    /// `tags` / `confidence` and a "Forget" button).
    async fn list_meta(&self, user_id: Uuid, limit: usize) -> Result<Vec<MemoryMeta>, AgentError>;

    /// Forget one memory by id. Cross-user attempts return
    /// `AgentError::AgentFailed("memory: cross-user forget refused")`
    /// so the LLM cannot tell "exists but other user" from "does
    /// not exist".
    async fn forget(&self, user_id: Uuid, id: Uuid) -> Result<(), AgentError>;
}

/// Plaintext input to [`MemorySource::store`].
///
/// Lives at the agents layer (not under `nagent_db`) because the
/// agents crate must not depend on `nagent_db` (the "agents do not
/// see the DB" layering rule, see `docs/architecture.md`
/// §Workspace layout). The `nagent-server` adapter encrypts each
/// `SecretString` field with the server-side
/// `[auth.credentials].key` and forwards the ciphertext to
/// `nagent_db::memories::Memories::upsert` as a
/// `nagent_db::NewMemoryRequest`.
#[derive(Clone)]
pub struct MemoryWriteRequest {
    pub subject: String,
    pub predicate: String,
    pub value: secrecy::SecretString,
    pub notes: Option<secrecy::SecretString>,
    pub tags: String,
    pub confidence: f32,
    pub source_session_id: Option<String>,
    pub source_kind: String,
}

impl std::fmt::Debug for MemoryWriteRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryWriteRequest")
            .field("subject", &self.subject)
            .field("predicate", &self.predicate)
            .field("value", &"<redacted SecretString>")
            .field(
                "notes",
                &self.notes.as_ref().map(|_| "<redacted SecretString>"),
            )
            .field("tags", &self.tags)
            .field("confidence", &self.confidence)
            .field("source_session_id", &self.source_session_id)
            .field("source_kind", &self.source_kind)
            .finish()
    }
}

/// Decrypted view of one memory row. Returned by
/// [`MemorySource::recall`].
///
/// The `value` and `notes` fields are `SecretString` so the
/// plaintext buffer zeroises on drop. The struct itself is `Debug`
/// but the `Display` impl (and the `tracing` "value" field)
/// intentionally never prints the plaintext — same posture as
/// `BasicAuth::Debug`.
#[derive(Clone)]
pub struct DecryptedMemory {
    pub id: Uuid,
    pub subject: String,
    pub predicate: String,
    pub value: secrecy::SecretString,
    pub notes: Option<secrecy::SecretString>,
    pub tags: String,
    pub confidence: f32,
    pub source_session_id: Option<String>,
    pub source_kind: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl std::fmt::Debug for DecryptedMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecryptedMemory")
            .field("id", &self.id)
            .field("subject", &self.subject)
            .field("predicate", &self.predicate)
            .field("value", &"<redacted SecretString>")
            .field(
                "notes",
                &self.notes.as_ref().map(|_| "<redacted SecretString>"),
            )
            .field("tags", &self.tags)
            .field("confidence", &self.confidence)
            .field("source_session_id", &self.source_session_id)
            .field("source_kind", &self.source_kind)
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Metadata-only view of a memory row. Returned by
/// [`MemorySource::list_meta`]. Mirrors the `nagent_db::MemoryMeta`
/// row shape verbatim; the `nagent-server` adapter does the trivial
/// field-for-field conversion so the agents crate never depends on
/// `nagent-db` (the "agents do not depend on nagent-db" layering
/// rule, see `docs/architecture.md` §"Workspace layout").
#[derive(Debug, Clone)]
pub struct MemoryMeta {
    pub id: Uuid,
    pub subject: String,
    pub predicate: String,
    pub tags: String,
    pub confidence: f32,
    pub source_session_id: Option<String>,
    pub source_kind: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
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
///
/// Plan 1791317253718: the LLM-facing surface is now a single
/// meta-agent ([`crate::agents::tool_search::ToolSearchAgent`])
/// plus a per-round BM25 pre-selection (see
/// [`crate::tools_router::ToolsRouter`]) plus the per-session
/// discovered-tools set on the server. All other registered
/// agents are reached through `search_tools` (or directly when
/// the router pre-selects them on a clear-fit query). The trait
/// shape is unchanged: a new agent just adds a descriptor entry,
/// and the round-level builder picks it up automatically.
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

    /// Extra indexable terms the BM25 pre-selection router should
    /// associate with this agent. Default = empty. Agents whose
    /// `name()` and `description()` already cover the obvious
    /// search terms (e.g. `get_weather`, `wikipedia`) leave this
    /// alone; agents whose canonical name does not match the
    /// natural-language query ("weather", "météo") list a few
    /// synonyms here so the router pre-selects them without
    /// forcing the LLM through `search_tools` first.
    fn keywords(&self) -> &'static [&'static str] {
        &[]
    }

    /// Post-build wiring hook called once per agent right after
    /// the registry is built. Default = no-op. The
    /// [`ToolSearchAgent`](crate::agents::tool_search::ToolSearchAgent)
    /// overrides this to attach the [`crate::tools_router::ToolsRouter`]
    /// the registry was indexed against, so its `invoke` can do
    /// the actual BM25 walk. The default keeps the trait free of
    /// any router / wiring concept for the (large) majority of
    /// agents that never need it.
    fn wire_router(&self, _router: std::sync::Arc<crate::tools_router::ToolsRouter>) {}
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
    /// `None` for `for_tests()` contexts and any agent path that
    /// has not been wired with a sink (plan 1790695073418). When
    /// `None`, [`UserContext::update_secret`] returns
    /// `AgentFailed("credential sink not wired in this context")`
    /// so refresh-style agents fail closed outside the chat-session
    /// constructor.
    sink: Option<Arc<dyn SecretSink>>,
    /// `None` for `for_tests()` contexts and any path that has
    /// not been wired with a memory source (the LLM tool loop
    /// outside the chat-session constructor, direct
    /// `/v1/agents/:name/invoke`, integration tests that do not
    /// exercise the memory subsystem). When `None`, the memory
    /// agents fail closed with
    /// `AgentError::AgentFailed("memory: source not wired in this
    /// context")`.
    ///
    /// Mirrors the `resolver` / `sink` pattern: production code
    /// that owns a `MemorySource` attaches it once at the chat
    /// boundary, and the four memory agents (`memory_store`,
    /// `memory_recall`, `memory_list`, `memory_forget`) reach for
    /// it through [`UserContext::memories`].
    memories: Option<Arc<dyn MemorySource>>,
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
            .field("sink", &self.sink.as_ref().map(|_| "<sink>"))
            .field(
                "memories",
                &self.memories.as_ref().map(|_| "<memory source>"),
            )
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
            sink: None,
            memories: None,
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
            sink: None,
            memories: None,
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
        sink: Option<Arc<dyn SecretSink>>,
        chat_session_id: Uuid,
    ) -> Self {
        Self {
            user_id,
            services,
            resolver,
            sink,
            memories: None,
            cache: crate::agents::credential_cache::SecretCache::new(),
            chat_session_id: Some(chat_session_id),
            invoked_this_turn: Vec::new(),
        }
    }

    /// Chat-session constructor with the [`MemorySource`] attached
    /// (plan 1791267136806, §2.2). The memory subsystem is
    /// optional — a build without the `memory-agent` cargo feature
    /// (or with the operator kill-switch
    /// `LLM_ALLOW_USER_MEMORY=false`) calls
    /// [`UserContext::for_chat_session`] and the `memory_*` agents
    /// simply never run. With the source wired, the four memory
    /// agents read / write the per-user encrypted store through
    /// [`UserContext::memories`].
    pub fn for_chat_session_with_memories(
        user_id: Uuid,
        services: Arc<ServiceRegistry>,
        resolver: Option<Arc<dyn SecretSource>>,
        sink: Option<Arc<dyn SecretSink>>,
        memory_source: Arc<dyn MemorySource>,
        chat_session_id: Uuid,
    ) -> Self {
        Self {
            user_id,
            services,
            resolver,
            sink,
            memories: Some(memory_source),
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

    /// Write back one or more `(field, plaintext)` rows under
    /// `service` for the calling user (plan 1790695073418 — the X
    /// OAuth "refresh handled by the agent itself" locked
    /// decision). The companion of [`Self::user`]; the impl is
    /// the inverse direction of [`SecretSource`] and lives next to
    /// it on the server side.
    ///
    /// Returns:
    /// - `Ok(())` after the new field set is persisted + audited;
    /// - `Err(AgentError::AgentFailed("credential sink not wired in
    ///   this context"))` when no sink is wired (test contexts,
    ///   direct-invoke routes); the agent should surface the error
    ///   verbatim so the LLM tells the user to reconnect X via
    ///   `/settings/integrations`;
    /// - `Err(AgentError::AgentFailed)` for encrypt / DB errors
    ///   surfaced by the server-side impl.
    pub async fn update_secret(
        &self,
        service: &str,
        fields: &[(&str, secrecy::SecretString)],
    ) -> Result<(), AgentError> {
        let Some(sink) = &self.sink else {
            return Err(AgentError::AgentFailed(
                "credential sink not wired in this context".into(),
            ));
        };
        sink.update(self.user_id, service, fields).await
    }

    /// Borrow the wired [`MemorySource`] (plan 1791267136806, §2.2).
    /// Returns `None` for any context that was not built with a
    /// memory source — test contexts, direct-invoke routes, and
    /// chat contexts where the operator disabled memory at boot
    /// (`LLM_ALLOW_USER_MEMORY=false`).
    ///
    /// The `memory_store` / `memory_list` / `memory_recall` /
    /// `memory_forget` agents refuse to run with
    /// `AgentError::AgentFailed("memory: source not wired in this
    /// context")` when this returns `None`, so the chat UI shows a
    /// clear "memory subsystem disabled" error rather than a
    /// confusing 500.
    pub fn memories(&self) -> Option<&dyn MemorySource> {
        self.memories.as_deref()
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
    /// the heavy machinery (one cargo feature per agent).
    ///
    /// `enabled` is the operator-controlled master switch; when
    /// `false` the function returns `Self::empty()` so the LLM
    /// proxy injects no `tools` field.
    ///
    /// `pool` is the shared `reqwest::Client` pool — every network
    /// agent is built with `EgressClient::from_shared_client(...)`
    /// against one of the pool's clients, so the connection pool
    /// and TLS roots stay warm across agents of the same policy
    /// class (plan 4.C).
    ///
    /// A descriptor that returns `Err` from its `build` closure
    /// is logged at `warn` and skipped — the rest of the registry
    /// still loads. Today every descriptor is infallible; the
    /// branch exists so a future agent can validate its config
    /// without breaking the others.
    pub fn from_config(
        cfgs: &crate::config::AgentConfigs,
        enabled: bool,
        pool: &EgressPool,
    ) -> Self {
        if !enabled {
            return Self::empty();
        }
        let mut registry = Self::empty();
        for descriptor in AGENT_DESCRIPTORS {
            match (descriptor.build)(cfgs, pool) {
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

    /// Post-build wiring hook. Walks every registered agent
    /// and hands it a clone of `router` so any meta-agent that
    /// consults a BM25 index (today: [`crate::agents::tool_search::ToolSearchAgent`])
    /// has the index in scope by the time the first
    /// `/v1/chat/completions` round starts. Agents that do not
    /// override [`Agent::wire_router`] receive the call as a
    /// no-op — the trait default keeps the trait free of any
    /// router concept for the (large) majority of agents that
    /// never need it. `None` for an empty registry (the loop is
    /// a no-op).
    pub fn wire_router(&self, router: std::sync::Arc<crate::tools_router::ToolsRouter>) {
        for agent in self.iter() {
            agent.wire_router(router.clone());
        }
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
// One line per agent. Adding a new agent is:
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
    /// Build a boxed instance from the global agent configs and
    /// the shared [`EgressPool`]. Returning `Err` is reserved
    /// for agents whose config is invalid (a missing API key,
    /// an unparseable base URL); the registry will skip the
    /// agent and log a warning. Today every entry is infallible;
    /// the `Result` exists so a future agent can fail gracefully
    /// without breaking the other descriptors.
    pub build:
        fn(&crate::config::AgentConfigs, &EgressPool) -> Result<Box<dyn Agent>, AgentBuildError>,
}

/// Static factory table. Walked by
/// [`AgentRegistry::from_config`]; every entry is gated by its
/// own cargo feature so a slim build drops the heavy machinery
/// (one cargo feature per agent).
pub static AGENT_DESCRIPTORS: &[AgentDescriptor] = &[
    #[cfg(feature = "web-agent")]
    AgentDescriptor {
        id: "web_fetch",
        feature: "web-agent",
        build: |cfgs, pool| {
            Ok(Box::new(web_fetch::WebFetchAgent::with_pool(
                pool.strict(),
                cfgs.web_fetch.clone(),
            )))
        },
    },
    #[cfg(feature = "datetime-agent")]
    AgentDescriptor {
        id: "get_datetime",
        feature: "datetime-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(datetime_agent::DateTimeAgent::from_config(
                cfgs.datetime.clone(),
            )))
        },
    },
    #[cfg(feature = "weather-agent")]
    AgentDescriptor {
        id: "get_weather",
        feature: "weather-agent",
        build: |cfgs, pool| {
            Ok(Box::new(weather_agent::WeatherAgent::with_shared_pool(
                pool.public(),
                cfgs.weather.clone(),
            )))
        },
    },
    #[cfg(feature = "stock-agent")]
    AgentDescriptor {
        id: "get_stock_quote",
        feature: "stock-agent",
        build: |cfgs, pool| {
            Ok(Box::new(stock_agent::StockAgent::with_shared_pool(
                pool.public(),
                cfgs.stock.clone(),
            )))
        },
    },
    #[cfg(feature = "calculate-agent")]
    AgentDescriptor {
        id: "calculate",
        feature: "calculate-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(calculate_agent::CalculateAgent::from_config(
                cfgs.calculate.clone(),
            )))
        },
    },
    #[cfg(feature = "unit-convert-agent")]
    AgentDescriptor {
        id: "unit_convert",
        feature: "unit-convert-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(unit_convert_agent::UnitConvertAgent::new(
                cfgs.unit_convert.clone(),
            )))
        },
    },
    #[cfg(feature = "wikipedia-agent")]
    AgentDescriptor {
        id: "wikipedia",
        feature: "wikipedia-agent",
        build: |cfgs, pool| {
            Ok(Box::new(wikipedia_agent::WikipediaAgent::with_shared_pool(
                pool.public(),
                cfgs.wikipedia.clone(),
            )))
        },
    },
    #[cfg(feature = "dictionary-agent")]
    AgentDescriptor {
        id: "dictionary",
        feature: "dictionary-agent",
        build: |cfgs, pool| {
            Ok(Box::new(
                dictionary_agent::DictionaryAgent::with_shared_pool(
                    pool.public(),
                    cfgs.dictionary.clone(),
                ),
            ))
        },
    },
    // Plan 1790963194218: CalDAV plugin (read + add only). The
    // three agents share the same `CalDavAgentConfig` and the
    // same per-call `CalDavClient` shape — built inside `invoke`
    // because credentials must be fresh per request. v1 does NOT
    // register `caldav_list_calendars`, `caldav_update_event`, or
    // `caldav_delete_event`; the LLM tool loop returns "unknown
    // tool" if it tries.
    #[cfg(feature = "caldav-agent")]
    AgentDescriptor {
        id: "caldav_list_events",
        feature: "caldav-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(caldav::list_events::ListEventsAgent::new(
                cfgs.caldav.clone(),
            )))
        },
    },
    #[cfg(feature = "caldav-agent")]
    AgentDescriptor {
        id: "caldav_get_event",
        feature: "caldav-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(caldav::get_event::GetEventAgent::new(
                cfgs.caldav.clone(),
            )))
        },
    },
    #[cfg(feature = "caldav-agent")]
    AgentDescriptor {
        id: "caldav_create_event",
        feature: "caldav-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(caldav::create_event::CreateEventAgent::new(
                cfgs.caldav.clone(),
            )))
        },
    },
    // Plan 1790695073418: X timeline agent. Read-only v1; refresh
    // is handled inside the agent itself when X returns 401
    // (locked decision). The shared client is `pool.public()`
    // because `api.x.com` is a public host (with the agent's
    // `allowlist` enforcing exactly that host).
    #[cfg(feature = "x-agent")]
    AgentDescriptor {
        id: "x_timeline",
        feature: "x-agent",
        build: |cfgs, pool| {
            Ok(Box::new(x_timeline::XTimelineAgent::new(
                cfgs.x_timeline.clone(),
                pool.public(),
            )))
        },
    },
    // Plan 1791267136806 §2.3: four memory agents (memory_store /
    // memory_recall / memory_list / memory_forget). They share the
    // `MemoryAgentConfig` knob (currently just `recalled_top_k`)
    // and the `MemorySource` capability (the per-request adapter
    // is wired by the chat-session constructor on the server
    // side). The agents themselves are stateless — every
    // store / recall / forget round-trips through the trait.
    #[cfg(feature = "memory-agent")]
    AgentDescriptor {
        id: "memory_store",
        feature: "memory-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(memory::MemoryStoreAgent::from_config(
                cfgs.memory.clone(),
            )))
        },
    },
    #[cfg(feature = "memory-agent")]
    AgentDescriptor {
        id: "memory_recall",
        feature: "memory-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(memory::MemoryRecallAgent::from_config(
                cfgs.memory.clone(),
            )))
        },
    },
    #[cfg(feature = "memory-agent")]
    AgentDescriptor {
        id: "memory_list",
        feature: "memory-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(memory::MemoryListAgent::from_config(
                cfgs.memory.clone(),
            )))
        },
    },
    #[cfg(feature = "memory-agent")]
    AgentDescriptor {
        id: "memory_forget",
        feature: "memory-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(memory::MemoryForgetAgent::from_config(
                cfgs.memory.clone(),
            )))
        },
    },
    // Plan 1791317253718: `search_tools` meta-agent. The router
    // is wired post-construction by the server's boot path
    // (`AgentRegistry::wire_router`); the descriptor table only
    // shapes the agent's config and registers the name. The
    // tool loop treats it like every other agent — the generic
    // `agents.get(&name)` dispatch is unchanged.
    #[cfg(feature = "tool-search-agent")]
    AgentDescriptor {
        id: "search_tools",
        feature: "tool-search-agent",
        build: |cfgs, _pool| {
            Ok(Box::new(tool_search::ToolSearchAgent::from_config(
                cfgs.tool_search.clone(),
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

    /// Plan 1790963194218: v1 ships **read + add only** for
    /// CalDAV. `caldav_update_event`, `caldav_delete_event`, and
    /// `caldav_list_calendars` must NOT be registered in
    /// `AGENT_DESCRIPTORS` — a model that tries to call them
    /// would receive `"unknown tool"`. The setup-only probe
    /// endpoint is the only path to calendar discovery.
    #[cfg(feature = "caldav-agent")]
    #[test]
    fn caldav_forbidden_tools_are_not_in_descriptor_table() {
        let forbidden = [
            "caldav_update_event",
            "caldav_delete_event",
            "caldav_list_calendars",
        ];
        let registered: std::collections::HashSet<&str> =
            AGENT_DESCRIPTORS.iter().map(|d| d.id).collect();
        for name in forbidden {
            assert!(
                !registered.contains(name),
                "`{name}` must not be registered as an LLM tool in v1 (read+add only); \
                 update/delete are forbidden and `caldav_list_calendars` is a setup-only \
                 HTTP endpoint, not a chat tool"
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
        let pool = EgressPool::new();
        for d in AGENT_DESCRIPTORS {
            let agent = (d.build)(&cfgs, &pool)
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

#[cfg(feature = "caldav-agent")]
pub mod caldav;

#[cfg(feature = "calculate-agent")]
pub mod calculate_agent;
#[cfg(feature = "datetime-agent")]
pub mod datetime_agent;
#[cfg(feature = "dictionary-agent")]
pub mod dictionary_agent;
#[cfg(feature = "memory-agent")]
pub mod memory;
#[cfg(feature = "read-document-agent")]
pub mod read_document;
#[cfg(feature = "stock-agent")]
pub mod stock_agent;
/// Plan 1791317253718: `search_tools` meta-agent. The BM25
/// router it consults lives in `crate::tools_router`. The
/// `tool-search-agent` feature follows the one-feature-per-
/// agent convention so a slim build drops both the agent and
/// its (small) router if it does not need tool discovery.
#[cfg(feature = "tool-search-agent")]
pub mod tool_search;
#[cfg(feature = "unit-convert-agent")]
pub mod unit_convert_agent;
#[cfg(feature = "weather-agent")]
pub mod weather_agent;
#[cfg(feature = "wikipedia-agent")]
pub mod wikipedia_agent;
#[cfg(feature = "x-agent")]
pub mod x_timeline;
