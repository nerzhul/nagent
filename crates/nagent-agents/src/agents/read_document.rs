//! `read_document` agent (plan 4.C).
//!
//! Lives in `nagent-agents` so adding a new agent is one directory,
//! one feature line, docs. Talks to the documents subsystem through
//! the [`DocumentSource`] trait — the agent itself has zero
//! dependency on `nagent_db` or the on-disk storage layout.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError, DocumentSource, UserContext};

/// `read_document` agent. Stateless — holds an `Arc<dyn
/// DocumentSource>` so each `invoke` can run a single document
/// fetch + read.
#[derive(Clone)]
pub struct ReadDocumentAgent {
    source: std::sync::Arc<dyn DocumentSource>,
}

impl std::fmt::Debug for ReadDocumentAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadDocumentAgent")
            .field("source", &"<dyn DocumentSource>")
            .finish()
    }
}

impl ReadDocumentAgent {
    /// Build from a [`DocumentSource`] implementation. The server
    /// hands in a wrapper around [`crate::documents::DocumentStore`]
    /// + [`crate::chat::sessions::ChatSessions`] so the agent sees
    /// only the narrow capability surface it needs.
    pub fn new(source: std::sync::Arc<dyn DocumentSource>) -> Self {
        Self { source }
    }
}

#[async_trait]
impl Agent for ReadDocumentAgent {
    fn name(&self) -> &str {
        "read_document"
    }

    fn description(&self) -> &str {
        "Read the text content of a document previously uploaded by the user to this chat session. \
         Use the `name` field returned by GET /v1/documents (a UUID). For PDFs, optionally restrict \
         to a `page_range` (e.g. \"3-7\") to limit context size. The tool returns at most \
         max_extracted_chars characters; if the document is truncated, ask the user for the \
         specific section you need."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Document id (UUID) returned by GET /v1/documents."
                },
                "page_range": {
                    "type": "string",
                    "description": "Optional, format 'N' or 'N-M'. Currently advisory; v1 returns the full extracted text up to max_extracted_chars."
                }
            },
            "required": ["name"],
            "additionalProperties": false,
        })
    }

    /// Documents are operator-controlled, not untrusted remote content.
    /// The LLM does not need the indirect-prompt-injection fence.
    fn untrusted_output(&self) -> bool {
        false
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let session_id = ctx.chat_session_id().ok_or_else(|| {
            AgentError::InvalidArguments(
                "read_document requires an active chat session id (X-Chat-Session-Id header)"
                    .to_string(),
            )
        })?;
        let payload = self
            .source
            .read(ctx.user_id(), session_id, &req.name)
            .await?;
        let max_chars = self.source.max_extracted_chars();
        let truncated = payload.text.chars().count() > max_chars;
        let (kept, marker) = if truncated {
            let kept: String = payload.text.chars().take(max_chars).collect();
            (kept, format!("\n[… truncated at {max_chars} characters …]"))
        } else {
            (payload.text, String::new())
        };
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "summary": format!(
                "{} ({} bytes, {} pages)",
                payload.original_name,
                payload.size_bytes,
                payload.page_count
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "?".into())
            ),
            "data": {
                "name": payload.id.to_string(),
                "original_name": payload.original_name,
                "mime": payload.mime,
                "text": format!("{kept}{marker}"),
                "page_range": req.page_range,
                "truncated": truncated,
                "extracted_chars": payload.extracted_chars,
            },
            "source": "local",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug)]
struct ParsedArgs {
    name: String,
    #[allow(dead_code)]
    page_range: Option<String>,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`name` (string) is required".into()))?
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`name` must not be empty".into(),
        ));
    }
    let page_range = obj
        .get("page_range")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(ref pr) = page_range {
        validate_page_range(pr)?;
    }
    Ok(ParsedArgs { name, page_range })
}

fn validate_page_range(pr: &str) -> Result<(), AgentError> {
    let parts: Vec<&str> = pr.split('-').collect();
    match parts.as_slice() {
        [n] => n.parse::<u32>().map(|_| ()).map_err(|_| {
            AgentError::InvalidArguments(format!(
                "invalid page_range `{pr}`; expected `N` or `N-M` (1-indexed)"
            ))
        }),
        [lo, hi] => {
            let lo_v: u32 = lo.parse().map_err(|_| {
                AgentError::InvalidArguments(format!(
                    "invalid page_range `{pr}`; expected `N` or `N-M` (1-indexed)"
                ))
            })?;
            let hi_v: u32 = hi.parse().map_err(|_| {
                AgentError::InvalidArguments(format!(
                    "invalid page_range `{pr}`; expected `N` or `N-M` (1-indexed)"
                ))
            })?;
            if lo_v == 0 || hi_v == 0 || lo_v > hi_v {
                return Err(AgentError::InvalidArguments(format!(
                    "invalid page_range `{pr}`; lo must be >= 1 and <= hi"
                )));
            }
            Ok(())
        }
        _ => Err(AgentError::InvalidArguments(format!(
            "invalid page_range `{pr}`; expected `N` or `N-M` (1-indexed)"
        ))),
    }
}
