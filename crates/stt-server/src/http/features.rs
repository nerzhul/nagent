//! `GET /api/features` — list enabled server features for the
//! frontend.
//!
//! Returns the set of optional subsystems the operator has turned
//! on at build + runtime. The browser fetches this once after
//! `/api/me` succeeds and uses the flags to gate UI sections
//! (the Documents panel, the Discussion-mode tab, the TTS
//! settings drawer, …). Without this endpoint the UI would
//! blindly render controls that 404 on every request when the
//! corresponding server subsystem is off — the user sees
//! broken UI rather than a curated feature surface.
//!
//! ## Wire shape
//!
//! ```http
//! GET /api/features
//! Cookie: nagent_session=<id>
//!
//! 200 OK
//! {
//!   "documents": true,          // [documents].enabled = true
//!   "llm": true,                // LLM proxy is wired
//!   "tts": true,                // TTS engine wired
//!   "agents": true,             // at least one agent registered
//!   "agent_names": ["get_weather", "read_document", ...],
//!   "chat_sessions": true,      // X-Chat-Session-Id binding wired
//!   "tools": ["get_weather", "read_document", ...]
//! }
//! ```
//!
//! The endpoint is authenticated (`RequireAuth` middleware) —
//! the response is not user-specific but the same envelope
//! already gates every other `/api/*` route. Cookies are
//! sufficient; CSRF is not checked because this is a `GET`.
//!
//! ## Build-time feature gates
//!
//! Some features are gated at compile time via cargo features
//! (e.g. `stt-server/tts` enables the Piper TTS engine, individual
//! `*-agent` features add specific LLM tools). The endpoint
//! reflects what's *actually* registered at runtime via the
//! `AgentRegistry` — there's no separate "build features" flag
//! because a build without the TTS cargo feature simply has no
//! TTS subsystem on `state.tts`.

use std::sync::Arc;

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::AppState;

/// Response shape of `GET /api/features`. Each flag is a boolean
/// indicating whether the corresponding subsystem is wired and
/// reachable from the request handler. The frontend reads this
/// once after `/api/me` succeeds and hides any UI element whose
/// flag is `false`.
///
/// New flags are additive — old clients ignore unknown fields
/// (serde default behaviour with `serde_json::Value` lookups
/// on the JS side). Renames are a breaking change.
#[derive(Debug, Clone, Serialize)]
pub struct Features {
    /// `[documents].enabled = true` AND the auth DB is
    /// reachable so the `uploaded_documents` table exists.
    /// The Discussion-mode Documents panel listens to this flag
    /// and removes itself from the DOM when `false`.
    #[serde(rename = "documents")]
    pub documents_enabled: bool,
    /// LLM proxy is wired (`state.llm.is_some()`). When false the
    /// Discussion-mode tab is hidden because every chat
    /// completion would fail with a 503.
    #[serde(rename = "llm")]
    pub llm_enabled: bool,
    /// TTS engine is wired (`state.tts.is_some()`). When false
    /// the TTS settings drawer is hidden and "Read aloud" is a
    /// no-op.
    #[serde(rename = "tts")]
    pub tts_enabled: bool,
    /// At least one LLM agent is registered (i.e. the
    /// `AgentRegistry` is non-empty AND `state.agents` is
    /// `Some`). Tool-call surfaces (the chat-pill badge, the
    /// agent toggle) hide when false.
    #[serde(rename = "agents")]
    pub agents_enabled: bool,
    /// Names of the agents actually wired (subset of
    /// `features.agents`). The frontend uses this to render
    /// per-agent UI hints (e.g. "weather card" mention only when
    /// `get_weather` is in the list).
    #[serde(rename = "agent_names")]
    pub agent_names: Vec<String>,
    /// Server-bound chat session id binding is wired
    /// (`state.chat_sessions.is_some()`). When false the browser
    /// falls back to a client-minted UUID for `X-Chat-Session-Id`
    /// (the server ignores it because no route reads it).
    #[serde(rename = "chat_sessions")]
    pub chat_sessions_enabled: bool,
    /// Names of every LLM tool currently exposed (subset of
    /// `features.agent_names`). Used by the chat-completions
    /// payload builder on the JS side so it knows what tools to
    /// advertise in the OpenAI `tools` array.
    #[serde(rename = "tools")]
    pub tools: Vec<String>,
}

impl Features {
    /// Build a `Features` snapshot from the current `AppState`.
    /// Called on every `GET /api/features` request — the data is
    /// small and the call is cheap, so we don't cache.
    pub fn from_state(state: &Arc<AppState>) -> Self {
        let documents_enabled = state.documents.is_some();
        let llm_enabled = state.llm.is_some();
        let tts_enabled = state.tts.is_some();
        let chat_sessions_enabled = state.chat_sessions.is_some();

        // Agent registry: empty when the master `agents.enabled`
        // toggle is off OR when no individual agent cargo
        // features are compiled in.
        let (agents_enabled, agent_names, tools) = match state.agents.as_ref() {
            Some(registry) => {
                let summaries = registry.list();
                let names: Vec<String> = summaries.iter().map(|s| s.name.clone()).collect();
                let tool_names: Vec<String> = summaries
                    .iter()
                    .filter(|s| !s.description.is_empty())
                    .map(|s| s.name.clone())
                    .collect();
                (!names.is_empty(), names, tool_names)
            }
            None => (false, Vec::new(), Vec::new()),
        };

        Self {
            documents_enabled,
            llm_enabled,
            tts_enabled,
            agents_enabled,
            agent_names,
            chat_sessions_enabled,
            tools,
        }
    }
}

/// `GET /api/features` — authenticated. Returns the current
/// `Features` snapshot. Cached by the browser for the lifetime
/// of the page (no auto-refresh) — operators who flip a feature
/// at runtime need a server restart (or the operator can call the
/// endpoint manually).
///
/// Takes the full `Arc<AppState>` on purpose: it intentionally
/// reads every optional sub-state to build the snapshot. This is
/// the only handler outside `http/` and `app.rs` that names
/// `AppState`.
pub async fn features_handler(State(state): State<Arc<AppState>>) -> Json<Features> {
    Json(Features::from_state(&state))
}

/// Build the `/api/features` router. Mounted under the protected
/// subtree so `RequireAuth` (when auth is enabled) gates it; on
/// the `auth.enabled = false` trust boundary the route is
/// publicly reachable (the response is not user-specific so this
/// is safe).
pub fn build_features_router(state: Arc<AppState>) -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/api/features", axum::routing::get(features_handler))
        .with_state(state)
}
