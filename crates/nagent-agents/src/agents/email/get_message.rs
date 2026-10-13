//! `email_get_message` agent — fetch the full body of one message
//! by IMAP UID. Plan 1791566269874 Phase 3.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::email::{EmailImap, ImapConnectConfig};
use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};
use crate::egress::host_matches_allowlist;
use crate::EmailAgentConfig;

use super::list_inbox::build_imap_config;

#[derive(Clone)]
pub struct GetMessageAgent {
    cfg: std::sync::Arc<EmailAgentConfig>,
    imap: Option<std::sync::Arc<dyn EmailImap>>,
    allowlist: Vec<String>,
}

impl std::fmt::Debug for GetMessageAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GetMessageAgent")
            .field("cfg", &self.cfg)
            .field("imap", &self.imap.as_ref().map(|_| "<dyn EmailImap>"))
            .field("allowlist", &self.allowlist)
            .finish()
    }
}

impl GetMessageAgent {
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
impl Agent for GetMessageAgent {
    fn name(&self) -> &str {
        "email_get_message"
    }

    fn description(&self) -> &str {
        "Fetch the full body of one message from the user's IMAP mailbox by its UID. Returns JSON \
         with `message` (uid, folder, from, to, cc, subject, date, body?, attachments[]) where \
         the body is plain-text and capped at `[agents.email].max_body_bytes`. v1 never decodes \
         attachment bytes — `attachments[]` is metadata only. Read operations on a third-party \
         service with the user's credentials are gated by the inline approval card."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "uid": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "IMAP UID returned by `email_list_inbox` or `email_search`. Required."
                },
                "folder": {
                    "type": "string",
                    "description": "Mailbox folder. Default `INBOX`."
                },
            },
            "required": ["uid"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        // Synonyms for the natural-language query "show
        // me an email" / "read the message about X" /
        // "open the email from Y". See the comment on
        // `list_inbox::keywords` for the full rationale.
        &[
            "email", "emails", "mail", "inbox", "messages", "read", "open", "show", "get", "fetch",
            "body", "content", "imap",
        ]
    }

    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        // Strict per-call gate (plan 1791834996882). The
        // previous two-step pattern (first call refuses,
        // second runs) was the LLM-mediated bypass the user
        // wants closed — every call must require a fresh
        // user click. The session-override button on the
        // approval card remains the only click-skip path;
        // that bypass lives in the tool loop's `force_allow`
        // check, not here.
        ConfirmationDecision::NeedsConfirmation {
            reason: format!(
                "`{name}` reads one message from the user's IMAP mailbox. The body is \
                 sensitive (full message text). The user must explicitly confirm the read \
                 in chat before this call runs. Wait for the user to click \"Autoriser\" \
                 in the chat UI; the tool will execute on their next message and you \
                 will see its result then.",
                name = self.name(),
            ),
        }
    }

    fn untrusted_output(&self) -> bool {
        // Message bodies are user-PII; treat as untrusted.
        true
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
        let message = imap
            .get_message(ctx.user_id(), &config, &req.folder, req.uid)
            .await?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": { "message": message }
        }))
        .expect("json encode"))
    }
}

#[derive(Debug)]
struct ParsedArgs {
    uid: u32,
    folder: String,
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
    Ok(ParsedArgs { uid, folder })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceRegistry;
    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn requires_confirmation_is_strict_even_in_same_turn() {
        // Plan 1791834996882: same-turn re-call shortcut is
        // closed — both calls must require confirmation.
        let agent = GetMessageAgent::new(Arc::new(EmailAgentConfig::default()), None, Vec::new());
        let mut ctx = UserContext::for_tests(Uuid::new_v4(), Arc::new(ServiceRegistry::empty()));
        let d1 = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(matches!(d1, ConfirmationDecision::NeedsConfirmation { .. }));
        ctx.record_invocation(agent.name());
        let d2 = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(
            matches!(d2, ConfirmationDecision::NeedsConfirmation { .. }),
            "second call in the same turn must STILL require \
             confirmation (plan 1791834996882); got {d2:?}"
        );
    }

    #[test]
    fn parse_args_rejects_missing_uid() {
        let err = parse_args(&json!({})).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_zero_uid() {
        let err = parse_args(&json!({"uid": 0})).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_defaults_folder_to_inbox() {
        let args = parse_args(&json!({"uid": 42})).unwrap();
        assert_eq!(args.uid, 42);
        assert_eq!(args.folder, "INBOX");
    }
}
