//! `email_mark_read` agent — set or clear the `\Seen` flag on a
//! single message. Plan 1791566269874 Phase 4.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::email::{EmailImap, ImapConnectConfig};
use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};
use crate::egress::host_matches_allowlist;
use crate::EmailAgentConfig;

use super::list_inbox::build_imap_config;

#[derive(Clone)]
pub struct MarkReadAgent {
    cfg: std::sync::Arc<EmailAgentConfig>,
    imap: Option<std::sync::Arc<dyn EmailImap>>,
    allowlist: Vec<String>,
}

impl std::fmt::Debug for MarkReadAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarkReadAgent")
            .field("cfg", &self.cfg)
            .field("imap", &self.imap.as_ref().map(|_| "<dyn EmailImap>"))
            .field("allowlist", &self.allowlist)
            .finish()
    }
}

impl MarkReadAgent {
    pub fn new(
        cfg: std::sync::Arc<EmailAgentConfig>,
        imap: Option<std::sync::Arc<dyn EmailImap>>,
        allowlist: Vec<String>,
    ) -> Self {
        Self {
            cfg,
            imap,
            allowlist,
        }
    }
}

#[async_trait]
impl Agent for MarkReadAgent {
    fn name(&self) -> &str {
        "email_mark_read"
    }

    fn description(&self) -> &str {
        "Set or clear the `\\Seen` flag on one message in the user's IMAP mailbox. The `read` \
         argument defaults to `true` (mark the message as read). Returns the new `read` state. \
         Mutating third-party state is gated by the inline approval card with session-override \
         (`Always allow` skips the gate for the rest of the session) — the same posture as \
         `caldav_create_event`."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "uid": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "IMAP UID. Required."
                },
                "folder": {
                    "type": "string",
                    "description": "Mailbox folder. Default `INBOX`."
                },
                "read": {
                    "type": "boolean",
                    "description": "Target `\\Seen` state. Default `true`."
                },
            },
            "required": ["uid"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        // Synonyms for the natural-language query "mark
        // the email as read" / "flag this as seen". See
        // the comment on `list_inbox::keywords` for the
        // full rationale.
        &[
            "email", "emails", "mail", "inbox", "messages", "mark", "read", "unread", "seen",
            "flag", "status", "imap",
        ]
    }

    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        // Flag-set operations on a third-party mailbox: gate
        // with the inline approval card. The LLM tool loop
        // already implements the session-override
        // ("Always allow ...") for any agent whose decision
        // is `NeedsConfirmation { .. }`, so a user who clicks
        // "Always allow `email_mark_read`" once skips the
        // gate for the rest of the session. This is the same
        // posture as `caldav_create_event`.
        ConfirmationDecision::NeedsConfirmation {
            reason: format!(
                "`{name}` mutates the user's IMAP mailbox (it sets or clears the `\\Seen` \
                 flag on one message). The user must confirm in chat before this call runs.",
                name = self.name(),
            ),
        }
    }

    fn untrusted_output(&self) -> bool {
        // The "ok" response is a small struct that does not
        // surface user PII.
        false
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let config: ImapConnectConfig = build_imap_config(ctx).await?;
        if self.allowlist.is_empty() || !host_matches_allowlist(&config.host, &self.allowlist) {
            return Err(AgentError::SandboxDenied(format!(
                "host `{}` is not in the email allowlist",
                config.host
            )));
        }
        let imap = self
            .imap
            .clone()
            .or_else(|| crate::agents::email::email_imap_impl().cloned());
        let Some(imap) = imap else {
            return Err(AgentError::AgentFailed(
                "email: IMAP capability not wired in this context".into(),
            ));
        };
        imap.set_flags(ctx.user_id(), &config, &req.folder, req.uid, req.read)
            .await?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": { "uid": req.uid, "folder": req.folder, "read": req.read }
        }))
        .expect("json encode"))
    }
}

#[derive(Debug)]
struct ParsedArgs {
    uid: u32,
    folder: String,
    read: bool,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let uid = obj
        .get("uid")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| AgentError::InvalidArguments("`uid` (integer) is required".into()))?
        as u32;
    if uid == 0 {
        return Err(AgentError::InvalidArguments(
            "`uid` must be a positive integer (IMAP UIDs are 1-indexed)".into(),
        ));
    }
    let folder = obj
        .get("folder")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "INBOX".to_string());
    let read = obj.get("read").and_then(|v| v.as_bool()).unwrap_or(true);
    Ok(ParsedArgs { uid, folder, read })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceRegistry;
    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn always_requires_confirmation_even_for_session_override() {
        // `email_mark_read` is confirm-on-write; the LLM tool
        // loop applies the session-override on top of that.
        // We assert the agent declares `NeedsConfirmation`
        // unconditionally; the session-override is a tool-loop
        // concern, not an agent one.
        let agent = MarkReadAgent::new(Arc::new(EmailAgentConfig::default()), None, Vec::new());
        let ctx = UserContext::for_tests(Uuid::new_v4(), Arc::new(ServiceRegistry::empty()));
        let d = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(matches!(d, ConfirmationDecision::NeedsConfirmation { .. }));
    }

    #[test]
    fn parse_args_defaults_read_to_true() {
        let args = parse_args(&json!({"uid": 7})).unwrap();
        assert!(args.read);
    }

    #[test]
    fn parse_args_rejects_missing_uid() {
        let err = parse_args(&json!({})).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }
}
