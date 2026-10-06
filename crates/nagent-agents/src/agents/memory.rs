//! Four `memory_*` chat agents (plan 1791267136806).
//!
//! - `memory_store` — persist one fact the user explicitly stated.
//! The system prompt tells the LLM when to call this (plan §1.1);
//! the call is `Allow` because it is idempotent through the
//! `(subject, predicate)` dedup + reversible through `memory_forget`.
//! - `memory_recall` — pull the top-K decrypted rows by
//! subject / predicate / tags, mostly used by the LLM to look
//! up a single fact when the auto-injected system block did
//! not contain it.
//! - `memory_list` — metadata listing (Settings-tab shape). The
//! LLM uses it when asked "list everything you've stored for
//! me" — the metadata does NOT include the encrypted value, so
//! the agent surfaces a taxonomy-only summary.
//! - `memory_forget` — destructive: `NeedsConfirmation` (even
//! though reversible) so the user sees a clear "are you sure?"
//! bubble before a row disappears.
//!
//! The agents reach the per-user encrypted store through the
//! [`MemorySource`] capability on [`UserContext`]; the encryption
//! key lives server-side, in `nagent_server::credentials`, and the
//! `MemorySource` impl on the server side calls into it.

use async_trait::async_trait;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};
use crate::config::MemoryAgentConfig;

/// Shared error message for the four agents when the chat context
/// was not wired with a `MemorySource` (test contexts, direct-
/// invoke routes, and operator kill-switch
/// `LLM_ALLOW_USER_MEMORY=false`). Surfacing a single string keeps
/// the chat UI's error mapping stable.
const MEMORY_NOT_WIREDATED: &str = "memory is disabled in user preferences";

fn source_unavailable(ctx: &UserContext) -> AgentError {
    if ctx.memories().is_none() {
        AgentError::AgentFailed(format!("memory: source not wired in this context"))
    } else {
        AgentError::AgentFailed(MEMORY_NOT_WIREDATED.to_string())
    }
}

// ============================================================================
// memory_store
// ============================================================================

/// Persist one fact for the calling user.
#[derive(Clone)]
pub struct MemoryStoreAgent;

impl std::fmt::Debug for MemoryStoreAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStoreAgent").finish()
    }
}

impl Default for MemoryStoreAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStoreAgent {
    pub fn new() -> Self {
        Self
    }

    /// Uniform constructor for the static factory table.
    pub fn from_config(_cfg: MemoryAgentConfig) -> Self {
        Self::new()
    }
}

#[async_trait]
impl Agent for MemoryStoreAgent {
    fn name(&self) -> &str {
        "memory_store"
    }

    fn description(&self) -> &str {
        "Persist a durable fact the user stated about themselves, their preferences, or their \
         relationships (e.g. \"my doctor is Dr Martin\", \"I'm allergic to penicillin\"). \
         The fact is encrypted at rest and surfaced automatically in subsequent chats via the \
         `memory_recall` block of the system prompt. Pass `subject` (categorical key like \
         \"doctor\" / \"spouse\" / \"user\"), `predicate` (categorical key like \"name\" / \
         \"allergy\" / \"birthday\"), `value` (the fact itself). Optional: `notes`, `tags` \
         (comma-separated), `confidence` (0.0..1.0, default 1.0), `source_kind` \
         (\"user_stated\" | \"llm_inferred\", default \"user_stated\"). Store is \
         idempotent on (subject, predicate)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": {
                    "type": "string",
                    "description": "Categorical key for the fact (e.g. \"doctor\", \"spouse\", \"user\")."
                },
                "predicate": {
                    "type": "string",
                    "description": "Categorical key for the slot (e.g. \"name\", \"allergy\", \"birthday\")."
                },
                "value": {
                    "type": "string",
                    "description": "The fact itself (the PII). Stored AES-256-GCM encrypted."
                },
                "notes": {
                    "type": "string",
                    "description": "Optional free-form context (also encrypted at rest)."
                },
                "tags": {
                    "type": "string",
                    "description": "Optional comma-separated tags (e.g. \"medical,family\"). Plaintext, used for LIKE filtering."
                },
                "confidence": {
                    "type": "number",
                    "description": "Optional confidence in [0.0, 1.0]. Defaults to 1.0.",
                    "minimum": 0.0,
                    "maximum": 1.0
                },
                "source_kind": {
                    "type": "string",
                    "enum": ["user_stated", "llm_inferred"],
                    "description": "Optional provenance tag. Defaults to \"user_stated\"."
                }
            },
            "required": ["subject", "predicate", "value"],
            "additionalProperties": false,
        })
    }

    fn untrusted_output(&self) -> bool {
        // The agent writes its own user's facts; there is no
        // remote-content prompt-injection surface.
        false
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_store_args(&args)?;
        let source = ctx.memories().ok_or_else(|| source_unavailable(ctx))?;
        let session_id = ctx.chat_session_id().map(|u| u.to_string());
        // Capture the user-visible fields for the JSON response
        // BEFORE moving the write request (which consumes the
        // `SecretString`s). The `id` returned by the store is
        // what the LLM needs to call `memory_recall` /
        // `memory_forget` against; we echo the rest back so the
        // chat UI can render a "stored: doctor.name = Dr Martin"
        // confirmation card without a follow-up `memory_recall`.
        let resp_subject = req.subject.clone();
        let resp_predicate = req.predicate.clone();
        let resp_tags = req.tags.clone();
        let resp_source_kind = req.source_kind.clone();
        let value_secret = secrecy::SecretString::new(req.value.into_boxed_str());
        let notes_secret = req
            .notes
            .map(|s| secrecy::SecretString::new(s.into_boxed_str()));
        let write_req = crate::agents::MemoryWriteRequest {
            subject: req.subject,
            predicate: req.predicate,
            value: value_secret,
            notes: notes_secret,
            tags: req.tags,
            confidence: req.confidence,
            source_session_id: session_id,
            source_kind: req.source_kind,
        };
        let id = source.store(ctx.user_id(), write_req).await?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": {
                "memory_id": id.to_string(),
                "subject": resp_subject,
                "predicate": resp_predicate,
                "tags": resp_tags,
                "source_kind": resp_source_kind,
            },
            "source": "memory",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ============================================================================
// memory_recall
// ============================================================================

/// Return up to `recalled_top_k` decrypted memories matching the
/// supplied filters. The LLM uses this when the auto-injected
/// system block (top-K per turn) didn't include the fact it needs.
#[derive(Clone)]
pub struct MemoryRecallAgent {
    recalled_top_k: usize,
}

impl std::fmt::Debug for MemoryRecallAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryRecallAgent")
            .field("recalled_top_k", &self.recalled_top_k)
            .finish()
    }
}

impl MemoryRecallAgent {
    pub fn from_config(cfg: MemoryAgentConfig) -> Self {
        Self {
            recalled_top_k: cfg.recalled_top_k,
        }
    }
}

#[async_trait]
impl Agent for MemoryRecallAgent {
    fn name(&self) -> &str {
        "memory_recall"
    }

    fn description(&self) -> &str {
        "Look up specific memories for the current user. Returns decrypted rows so you can use \
         them in your reply. Optional filters: `subject` (LIKE, % wildcards), `predicate` (LIKE, \
         %), `tags` (LIKE, %). Optional `limit` (defaults to the operator-configured top-K, \
         hard-capped at 64). Each row carries: id, subject, predicate, value, notes, tags, \
         confidence, source_session_id, last_used_at."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": {
                    "type": "string",
                    "description": "Optional LIKE filter on the subject column (e.g. \"doctor\")."
                },
                "predicate": {
                    "type": "string",
                    "description": "Optional LIKE filter on the predicate column (e.g. \"name\")."
                },
                "tags": {
                    "type": "string",
                    "description": "Optional LIKE filter on the tags column (e.g. \"%medical%\")."
                },
                "limit": {
                    "type": "integer",
                    "description": "Optional upper bound on rows returned (default = operator top-K, hard-capped at 64).",
                    "minimum": 1,
                    "maximum": 64
                }
            },
            "additionalProperties": false,
        })
    }

    fn untrusted_output(&self) -> bool {
        // The agent surfaces the user's own facts. Even though
        // those facts are plaintext, the LLM is already reading
        // the same set in the auto-injected system block — no
        // additional prompt-injection vector from the tool result.
        false
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_recall_args(&args)?;
        let source = ctx.memories().ok_or_else(|| source_unavailable(ctx))?;
        let limit = req.limit.unwrap_or(self.recalled_top_k);
        let rows = source
            .recall(
                ctx.user_id(),
                req.subject.as_deref(),
                req.predicate.as_deref(),
                req.tags.as_deref(),
                limit,
            )
            .await?;
        let data: Vec<Value> = rows
            .iter()
            .map(|m| {
                json!({
                    "id": m.id.to_string(),
                    "subject": m.subject,
                    "predicate": m.predicate,
                    "value": m.value.expose_secret(),
                    "notes": m.notes.as_ref().map(|n| n.expose_secret().to_string()),
                    "tags": m.tags,
                    "confidence": m.confidence,
                    "source_session_id": m.source_session_id,
                    "source_kind": m.source_kind,
                    "created_at": m.created_at.to_rfc3339(),
                    "last_used_at": m.last_used_at.map(|d| d.to_rfc3339()),
                    "expires_at": m.expires_at.map(|d| d.to_rfc3339()),
                })
            })
            .collect();
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": data,
            "count": rows.len(),
            "source": "memory",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ============================================================================
// memory_list
// ============================================================================

/// List metadata-only rows for the calling user. The LLM uses this
/// when asked "what have you stored for me?". The returned rows do
/// NOT include the encrypted `value` field (the auto-prompt path
/// and the SPA audit list both want the taxonomy without leaking
/// plaintext into an audit card).
#[derive(Clone)]
pub struct MemoryListAgent;

impl std::fmt::Debug for MemoryListAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryListAgent").finish()
    }
}

impl Default for MemoryListAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryListAgent {
    pub fn new() -> Self {
        Self
    }

    pub fn from_config(_cfg: MemoryAgentConfig) -> Self {
        Self::new()
    }
}

#[async_trait]
impl Agent for MemoryListAgent {
    fn name(&self) -> &str {
        "memory_list"
    }

    fn description(&self) -> &str {
        "List the durable facts currently stored for the current user. Returns metadata only \
         (subject / predicate / tags / confidence / source_kind / created_at / last_used_at) \
         — NO value or notes, so this cannot be used to dump plaintext. Optional `limit` \
         (default 64). Pair with `memory_recall` to retrieve the value of a specific id."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "description": "Optional upper bound on rows returned (default 64).",
                    "minimum": 1,
                    "maximum": 64
                }
            },
            "additionalProperties": false,
        })
    }

    fn untrusted_output(&self) -> bool {
        // Same posture as `memory_recall`: the LLM is seeing its own
        // user's taxonomy, not remote content.
        false
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_list_args(&args)?;
        let source = ctx.memories().ok_or_else(|| source_unavailable(ctx))?;
        let limit = req.limit.unwrap_or(64);
        let rows = source.list_meta(ctx.user_id(), limit).await?;
        let data: Vec<Value> = rows
            .iter()
            .map(|m| {
                json!({
                    "id": m.id.to_string(),
                    "subject": m.subject,
                    "predicate": m.predicate,
                    "tags": m.tags,
                    "confidence": m.confidence,
                    "source_session_id": m.source_session_id,
                    "source_kind": m.source_kind,
                    "created_at": m.created_at.to_rfc3339(),
                    "last_used_at": m.last_used_at.map(|d| d.to_rfc3339()),
                    "expires_at": m.expires_at.map(|d| d.to_rfc3339()),
                })
            })
            .collect();
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": data,
            "count": rows.len(),
            "source": "memory",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ============================================================================
// memory_forget
// ============================================================================

/// Forget one memory by id. Destructive in the UX sense (the row
/// disappears), even though `memory_store` could re-create it:
/// the plan §2.3 contract is `NeedsConfirmation` so the chat UI
/// surfaces a "are you sure?" bubble before the deletion lands.
#[derive(Clone)]
pub struct MemoryForgetAgent;

impl std::fmt::Debug for MemoryForgetAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryForgetAgent").finish()
    }
}

impl Default for MemoryForgetAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryForgetAgent {
    pub fn new() -> Self {
        Self
    }

    pub fn from_config(_cfg: MemoryAgentConfig) -> Self {
        Self::new()
    }
}

#[async_trait]
impl Agent for MemoryForgetAgent {
    fn name(&self) -> &str {
        "memory_forget"
    }

    fn description(&self) -> &str {
        "Forget (delete) one previously-stored memory. Requires an explicit user confirmation \
         before the row is removed. Pass the `memory_id` returned by `memory_store` or \
         `memory_list`."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "memory_id": {
                    "type": "string",
                    "description": "UUID of the memory row to forget."
                }
            },
            "required": ["memory_id"],
            "additionalProperties": false,
        })
    }

    fn untrusted_output(&self) -> bool {
        false
    }

    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        // Plan §2.3: destructive in the UX sense (the row
        // disappears and `memory_store` would have to re-create
        // it from scratch). The reason string is surfaced to the
        // LLM verbatim so the next round sees a clear
        // "Utilisateur refusé" on `Deny`.
        ConfirmationDecision::NeedsConfirmation {
            reason:
                "memory_forget permanently removes the row; confirm before proceeding so the user \
                 sees a 'forget this memory?' bubble."
                    .to_string(),
        }
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_forget_args(&args)?;
        let source = ctx.memories().ok_or_else(|| source_unavailable(ctx))?;
        source.forget(ctx.user_id(), req.memory_id).await?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": {
                "memory_id": req.memory_id.to_string(),
            },
            "source": "memory",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ============================================================================
// Argument parsing
// ============================================================================

#[derive(Debug)]
struct StoreArgs {
    subject: String,
    predicate: String,
    value: String,
    notes: Option<String>,
    tags: String,
    confidence: f32,
    source_kind: String,
}

fn parse_store_args(args: &Value) -> Result<StoreArgs, AgentError> {
    let obj = args.as_object().ok_or_else(|| {
        AgentError::InvalidArguments("memory_store arguments must be a JSON object".into())
    })?;
    let subject = obj
        .get("subject")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`subject` (string) is required".into()))?
        .trim()
        .to_string();
    if subject.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`subject` must not be empty".into(),
        ));
    }
    let predicate = obj
        .get("predicate")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`predicate` (string) is required".into()))?
        .trim()
        .to_string();
    if predicate.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`predicate` must not be empty".into(),
        ));
    }
    let value = obj
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`value` (string) is required".into()))?
        .trim()
        .to_string();
    if value.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`value` must not be empty".into(),
        ));
    }
    let notes = obj
        .get("notes")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tags = obj
        .get("tags")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let confidence = obj
        .get("confidence")
        .and_then(|v| v.as_f64())
        .map(|v| v as f32)
        .unwrap_or(1.0);
    if !(0.0..=1.0).contains(&confidence) || !confidence.is_finite() {
        return Err(AgentError::InvalidArguments(format!(
            "`confidence` must be a finite number in [0.0, 1.0]; got {confidence}"
        )));
    }
    let source_kind = obj
        .get("source_kind")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "user_stated".to_string());
    if source_kind != "user_stated" && source_kind != "llm_inferred" {
        return Err(AgentError::InvalidArguments(format!(
            "`source_kind` must be \"user_stated\" or \"llm_inferred\"; got {source_kind:?}"
        )));
    }
    Ok(StoreArgs {
        subject,
        predicate,
        value,
        notes,
        tags,
        confidence,
        source_kind,
    })
}

#[derive(Debug, Default)]
struct RecallArgs {
    subject: Option<String>,
    predicate: Option<String>,
    tags: Option<String>,
    limit: Option<usize>,
}

fn parse_recall_args(args: &Value) -> Result<RecallArgs, AgentError> {
    let obj = args.as_object().ok_or_else(|| {
        AgentError::InvalidArguments("memory_recall arguments must be a JSON object".into())
    })?;
    let subject = obj
        .get("subject")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let predicate = obj
        .get("predicate")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tags = obj
        .get("tags")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let limit = obj
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    if let Some(l) = limit {
        if l == 0 {
            return Err(AgentError::InvalidArguments("`limit` must be >= 1".into()));
        }
    }
    Ok(RecallArgs {
        subject,
        predicate,
        tags,
        limit,
    })
}

#[derive(Debug, Default)]
struct ListArgs {
    limit: Option<usize>,
}

fn parse_list_args(args: &Value) -> Result<ListArgs, AgentError> {
    let obj = args.as_object().ok_or_else(|| {
        AgentError::InvalidArguments("memory_list arguments must be a JSON object".into())
    })?;
    let limit = obj
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    if let Some(l) = limit {
        if l == 0 {
            return Err(AgentError::InvalidArguments("`limit` must be >= 1".into()));
        }
    }
    Ok(ListArgs { limit })
}

#[derive(Debug)]
struct ForgetArgs {
    memory_id: uuid::Uuid,
}

fn parse_forget_args(args: &Value) -> Result<ForgetArgs, AgentError> {
    let obj = args.as_object().ok_or_else(|| {
        AgentError::InvalidArguments("memory_forget arguments must be a JSON object".into())
    })?;
    let raw = obj
        .get("memory_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`memory_id` (string) is required".into()))?
        .trim();
    let memory_id = uuid::Uuid::parse_str(raw).map_err(|_| {
        AgentError::InvalidArguments(format!("`memory_id` must be a UUID; got {raw:?}"))
    })?;
    Ok(ForgetArgs { memory_id })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::MemoryWriteRequest;
    use crate::agents::UserContext;
    use serde_json::json;
    use std::sync::Arc;

    fn test_ctx() -> UserContext {
        UserContext::for_tests(
            uuid::Uuid::new_v4(),
            Arc::new(crate::ServiceRegistry::empty()),
        )
    }

    #[test]
    fn name_and_schema_are_stable() {
        for (agent, expected_name) in [
            (
                Box::new(MemoryStoreAgent::new()) as Box<dyn Agent>,
                "memory_store",
            ),
            (
                Box::new(MemoryRecallAgent::from_config(MemoryAgentConfig::default()))
                    as Box<dyn Agent>,
                "memory_recall",
            ),
            (
                Box::new(MemoryListAgent::new()) as Box<dyn Agent>,
                "memory_list",
            ),
            (
                Box::new(MemoryForgetAgent::new()) as Box<dyn Agent>,
                "memory_forget",
            ),
        ] {
            assert_eq!(agent.name(), expected_name);
            let schema = agent.parameters_schema();
            assert_eq!(schema["type"], "object");
            assert!(schema["properties"].is_object());
        }
    }

    #[tokio::test]
    async fn memory_store_fails_closed_when_source_is_unwired() {
        // `for_tests()` builds a context with `memories = None` —
        // the agent must surface a clean error instead of panicking.
        let ctx = test_ctx();
        let err = MemoryStoreAgent::new()
            .invoke(
                &ctx,
                json!({"subject": "doctor", "predicate": "name", "value": "Dr Martin"}),
            )
            .await
            .expect_err("source missing");
        match err {
            AgentError::AgentFailed(msg) => assert!(
                msg.contains("source not wired"),
                "expected 'source not wired' error, got {msg:?}"
            ),
            other => panic!("expected AgentFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn memory_recall_fails_closed_when_source_is_unwired() {
        let ctx = test_ctx();
        let err = MemoryRecallAgent::from_config(MemoryAgentConfig::default())
            .invoke(&ctx, json!({}))
            .await
            .expect_err("source missing");
        assert!(matches!(err, AgentError::AgentFailed(_)));
    }

    #[tokio::test]
    async fn memory_list_fails_closed_when_source_is_unwired() {
        let ctx = test_ctx();
        let err = MemoryListAgent::new()
            .invoke(&ctx, json!({}))
            .await
            .expect_err("source missing");
        assert!(matches!(err, AgentError::AgentFailed(_)));
    }

    #[tokio::test]
    async fn memory_forget_requires_confirmation() {
        let agent = MemoryForgetAgent::new();
        let ctx = test_ctx();
        let decision = agent.requires_confirmation(
            &ctx,
            &json!({"memory_id": uuid::Uuid::new_v4().to_string()}),
        );
        assert!(
            matches!(decision, ConfirmationDecision::NeedsConfirmation { .. }),
            "memory_forget must require confirmation"
        );
    }

    #[tokio::test]
    async fn memory_forget_fails_closed_when_source_is_unwired() {
        let ctx = test_ctx();
        let err = MemoryForgetAgent::new()
            .invoke(&ctx, json!({"memory_id": uuid::Uuid::new_v4().to_string()}))
            .await
            .expect_err("source missing");
        assert!(matches!(err, AgentError::AgentFailed(_)));
    }

    #[test]
    fn parse_store_args_rejects_empty_value() {
        let err = parse_store_args(&json!({
            "subject": "doctor",
            "predicate": "name",
            "value": "   "
        }))
        .expect_err("empty value");
        match err {
            AgentError::InvalidArguments(msg) => assert!(msg.contains("value")),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn parse_store_args_rejects_unknown_source_kind() {
        let err = parse_store_args(&json!({
            "subject": "doctor",
            "predicate": "name",
            "value": "Dr Martin",
            "source_kind": "random"
        }))
        .expect_err("unknown source_kind");
        match err {
            AgentError::InvalidArguments(msg) => assert!(msg.contains("source_kind")),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn parse_store_args_rejects_out_of_range_confidence() {
        let err = parse_store_args(&json!({
            "subject": "doctor",
            "predicate": "name",
            "value": "Dr Martin",
            "confidence": 1.5
        }))
        .expect_err("oob confidence");
        match err {
            AgentError::InvalidArguments(msg) => assert!(msg.contains("confidence")),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn parse_forget_args_rejects_non_uuid_memory_id() {
        let err = parse_forget_args(&json!({"memory_id": "not-a-uuid"})).expect_err("non-uuid");
        match err {
            AgentError::InvalidArguments(msg) => assert!(msg.contains("UUID")),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn memory_write_request_redacts_in_debug() {
        // The Debug impl must not print plaintext into memory
        // debug output (defence-in-depth; the `SecretString`
        // already prevents accidental formatting, this is the second
        // line of defence).
        let req = MemoryWriteRequest {
            subject: "doctor".into(),
            predicate: "name".into(),
            value: secrecy::SecretString::new("Dr Martin".to_string().into_boxed_str()),
            notes: Some(secrecy::SecretString::new(
                "secret note".to_string().into_boxed_str(),
            )),
            tags: "medical".into(),
            confidence: 1.0,
            source_session_id: None,
            source_kind: "user_stated".into(),
        };
        let dbg = format!("{req:?}");
        assert!(dbg.contains("doctor"));
        assert!(!dbg.contains("Dr Martin"), "value leaked in Debug: {dbg}");
        assert!(!dbg.contains("secret note"), "notes leaked in Debug: {dbg}");
    }
}
