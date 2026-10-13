//! `email_send` agent — send a message through the user's SMTP
//! server. Plan 1791566269874 Phase 5.
//!
//! ## Hardening
//!
//! `email_send` is the strictest tool in the registry: the inline
//! approval card fires on every invocation and the per-session
//! "Always allow" override is **intentionally not wired** for it
//! (see plan §1.1). The server-side check on the operator-set
//! `recipient_allowlist` is the second gate: a misconfigured vault
//! row cannot be coerced into handing a message to an arbitrary
//! address.
//!
//! The `from_address` is always read from the per-user vault
//! (cached in `email_allowed_from` after the SMTP probe at save
//! time) — the LLM never picks the sender.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::email::{EmailSmtp, OutgoingMessage, SmtpConnectConfig, SmtpTlsMode};
use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};
use crate::egress::host_matches_allowlist;
use crate::EmailAgentConfig;

#[derive(Clone)]
pub struct SendAgent {
    cfg: std::sync::Arc<EmailAgentConfig>,
    smtp: Option<std::sync::Arc<dyn EmailSmtp>>,
    allowlist: Vec<String>,
}

impl std::fmt::Debug for SendAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendAgent")
            .field("cfg", &self.cfg)
            .field("smtp", &self.smtp.as_ref().map(|_| "<dyn EmailSmtp>"))
            .field("allowlist", &self.allowlist)
            .finish()
    }
}

impl SendAgent {
    pub fn new(
        cfg: std::sync::Arc<EmailAgentConfig>,
        smtp: Option<std::sync::Arc<dyn EmailSmtp>>,
        allowlist: Vec<String>,
    ) -> Self {
        Self {
            cfg,
            smtp,
            allowlist,
        }
    }
}

#[async_trait]
impl Agent for SendAgent {
    fn name(&self) -> &str {
        "email_send"
    }

    fn description(&self) -> &str {
        "Send a message through the user's SMTP server. The `to` / `cc` / `bcc` recipients are \
         validated against the operator-set `recipient_allowlist` (default = the user's own \
         `from_address`) **before** the agent reaches the network. The `from_address` is read \
         from the per-user vault, never from the LLM. The inline approval card fires on every \
         invocation; the per-session `Always allow` override is intentionally **not** wired for \
         this tool — sending mail leaves the operator's perimeter, so the user must confirm \
         each message. The SMTP wire client connects per call (no persistent state on the \
         server)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient address (RFC 5322). Required."
                },
                "subject": {
                    "type": "string",
                    "description": "Subject line. Required."
                },
                "body": {
                    "type": "string",
                    "description": "Plain-text body. Required."
                },
                "cc": {
                    "type": "string",
                    "description": "Comma-separated CC recipients. Optional."
                },
                "bcc": {
                    "type": "string",
                    "description": "Comma-separated BCC recipients. Optional."
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "RFC 5322 `In-Reply-To:` value. Optional."
                },
                "references": {
                    "type": "string",
                    "description": "RFC 5322 `References:` value. Optional."
                },
            },
            "required": ["to", "subject", "body"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        // Synonyms for the natural-language query "send
        // an email to Alice" / "write a mail about X" /
        // "compose a message". See the comment on
        // `list_inbox::keywords` for the full rationale.
        &[
            "email", "emails", "mail", "send", "write", "compose", "reply", "forward", "outbox",
            "message", "smtp",
        ]
    }

    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        // Strictest tool in the registry. The inline approval
        // card fires on every invocation. The per-session
        // "Always allow" override is intentionally **not**
        // wired for this tool — see the LLM tool loop's
        // `should_skip_inline_card` for the enforcement
        // surface (the tool loop reads
        // `agent.requires_confirmation` AND checks the agent
        // name against the override set; `email_send` is added
        // to a small "no override" set on the server side).
        ConfirmationDecision::NeedsConfirmation {
            reason: format!(
                "`{name}` sends a real email through the user's mailbox. The user must \
                 explicitly confirm each send in chat — there is no per-session `Always \
                 allow` for this tool.",
                name = self.name(),
            ),
        }
    }

    fn untrusted_output(&self) -> bool {
        // The "ok" envelope does not surface PII.
        false
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let config = build_smtp_config(ctx).await?;
        if self.allowlist.is_empty() || !host_matches_allowlist(&config.host, &self.allowlist) {
            return Err(AgentError::SandboxDenied(format!(
                "host `{}` is not in the email allowlist",
                config.host
            )));
        }
        // Server-side recipient allow-list check (plan §1.1,
        // §4). The operator-set list is the second gate: a
        // misconfigured vault cannot be coerced into handing
        // a message to an arbitrary address.
        let allowed = effective_recipient_allowlist(ctx, &self.cfg).await;
        reject_disallowed_recipients(&req.to, &req.cc, &req.bcc, &allowed)?;
        let smtp = self
            .smtp
            .clone()
            .or_else(|| crate::agents::email::email_smtp_impl().cloned());
        let Some(smtp) = smtp else {
            return Err(AgentError::AgentFailed(
                "email: SMTP capability not wired in this context".into(),
            ));
        };
        let message = OutgoingMessage {
            from: config.from_address.clone(),
            to: req.to.clone(),
            cc: req.cc.clone(),
            bcc: req.bcc.clone(),
            subject: req.subject.clone(),
            body: req.body.clone(),
            in_reply_to: req.in_reply_to.clone(),
            references: req.references.clone(),
        };
        smtp.send(ctx.user_id(), &config, &message).await?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": {
                "from": config.from_address,
                "to": req.to,
                "subject": req.subject,
            }
        }))
        .expect("json encode"))
    }
}

#[derive(Debug)]
struct ParsedArgs {
    to: Vec<String>,
    subject: String,
    body: String,
    cc: Vec<String>,
    bcc: Vec<String>,
    in_reply_to: Option<String>,
    references: Option<String>,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let to_raw = obj
        .get("to")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`to` (string) is required".into()))?;
    let to = parse_recipient_list(to_raw)?;
    if to.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`to` must contain at least one recipient".into(),
        ));
    }
    let subject = obj
        .get("subject")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`subject` (string) is required".into()))?
        .to_string();
    let body = obj
        .get("body")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`body` (string) is required".into()))?
        .to_string();
    let cc = obj
        .get("cc")
        .and_then(|v| v.as_str())
        .map(parse_recipient_list)
        .transpose()?
        .unwrap_or_default();
    let bcc = obj
        .get("bcc")
        .and_then(|v| v.as_str())
        .map(parse_recipient_list)
        .transpose()?
        .unwrap_or_default();
    let in_reply_to = obj
        .get("in_reply_to")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let references = obj
        .get("references")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(ParsedArgs {
        to,
        subject,
        body,
        cc,
        bcc,
        in_reply_to,
        references,
    })
}

/// Parse a comma-separated recipient list. Empty entries are
/// dropped; each entry is trimmed of surrounding whitespace.
fn parse_recipient_list(raw: &str) -> Result<Vec<String>, AgentError> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if !s.contains('@') {
                return Err(AgentError::InvalidArguments(format!(
                    "recipient `{s}` is not a valid address"
                )));
            }
            Ok(s.to_string())
        })
        .collect()
}

/// Resolve the effective recipient allow-list. The agent's
/// `recipient_allowlist` is the operator-set list (the
/// `EmailAgentConfig::recipient_allowlist` field, default =
/// user's own `from_address`).
async fn effective_recipient_allowlist(ctx: &UserContext, cfg: &EmailAgentConfig) -> Vec<String> {
    if !cfg.recipient_allowlist.is_empty() {
        return cfg.recipient_allowlist.clone();
    }
    // Default: only the user's own from_address. Read it from
    // the per-user vault (the SMTP probe validates it at save
    // time; see `email_allowed_from` migration).
    if let Ok(Some(from)) = ctx.secret("email_smtp", "from_address").await {
        use secrecy::ExposeSecret;
        vec![from.expose_secret().to_string()]
    } else {
        Vec::new()
    }
}

/// Reject any recipient outside the operator allow-list. The
/// comparison is case-insensitive on the local part + domain
/// (the LLM may type `Alice@Example.com` even though the
/// allow-list stores `alice@example.com`).
fn reject_disallowed_recipients(
    to: &[String],
    cc: &[String],
    bcc: &[String],
    allowed: &[String],
) -> Result<(), AgentError> {
    if allowed.is_empty() {
        return Err(AgentError::SandboxDenied(
            "email_send: operator has not configured a recipient_allowlist and the user's \
             from_address is not set; refusing to send"
                .into(),
        ));
    }
    for recipient in to.iter().chain(cc.iter()).chain(bcc.iter()) {
        let lower = recipient.to_ascii_lowercase();
        if !allowed.iter().any(|a| a.to_ascii_lowercase() == lower) {
            return Err(AgentError::SandboxDenied(format!(
                "email_send: recipient `{recipient}` is not in the operator allow-list"
            )));
        }
    }
    Ok(())
}

async fn build_smtp_config(ctx: &UserContext) -> Result<SmtpConnectConfig, AgentError> {
    let host = ctx
        .secret("email_smtp", "host")
        .await?
        .ok_or_else(|| missing_field("host"))?;
    let port = ctx
        .secret("email_smtp", "port")
        .await?
        .ok_or_else(|| missing_field("port"))?;
    let tls_mode = ctx
        .secret("email_smtp", "tls_mode")
        .await?
        .ok_or_else(|| missing_field("tls_mode"))?;
    let username = ctx
        .secret("email_smtp", "username")
        .await?
        .ok_or_else(|| missing_field("username"))?;
    let password = ctx
        .secret("email_smtp", "password")
        .await?
        .ok_or_else(|| missing_field("password"))?;
    let from_address = ctx
        .secret("email_smtp", "from_address")
        .await?
        .ok_or_else(|| missing_field("from_address"))?;
    use secrecy::ExposeSecret;
    let host_s = host.expose_secret().to_string();
    let port_num: u16 =
        port.expose_secret().trim().parse().map_err(|_| {
            AgentError::InvalidArguments("email_smtp.port must be an integer".into())
        })?;
    let tls_mode = SmtpTlsMode::parse(tls_mode.expose_secret())?;
    Ok(SmtpConnectConfig {
        host: host_s,
        port: port_num,
        tls_mode,
        username: username.expose_secret().to_string(),
        password: password.expose_secret().to_string(),
        from_address: from_address.expose_secret().to_string(),
    })
}

fn missing_field(field: &str) -> AgentError {
    AgentError::AgentFailed(format!(
        "email_smtp credentials are not configured: set the `{field}` field in the Integrations page"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceRegistry;
    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn always_requires_confirmation_no_session_override() {
        // `email_send` is the strictest tool — the inline
        // approval card fires on every invocation and the
        // per-session "Always allow" override is **not**
        // wired for it. The agent declares
        // `NeedsConfirmation` unconditionally.
        let agent = SendAgent::new(Arc::new(EmailAgentConfig::default()), None, Vec::new());
        let ctx = UserContext::for_tests(Uuid::new_v4(), Arc::new(ServiceRegistry::empty()));
        let d = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(matches!(d, ConfirmationDecision::NeedsConfirmation { .. }));
    }

    #[test]
    fn parse_args_requires_to_subject_body() {
        let err = parse_args(&json!({})).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
        let err = parse_args(&json!({"to": "alice@example.com"})).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
        let err = parse_args(&json!({"to": "alice@example.com", "subject": "hi"})).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_address_without_at() {
        let err = parse_args(&json!({
            "to": "not-an-address",
            "subject": "hi",
            "body": "hello",
        }))
        .unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn reject_disallowed_recipients_blocks_unknown_address() {
        let allowed = vec!["alice@example.com".to_string()];
        let err =
            reject_disallowed_recipients(&["bob@example.com".to_string()], &[], &[], &allowed)
                .unwrap_err();
        assert!(matches!(err, AgentError::SandboxDenied(_)));
    }

    #[test]
    fn reject_disallowed_recipients_accepts_known_address_case_insensitive() {
        let allowed = vec!["alice@example.com".to_string()];
        reject_disallowed_recipients(&["Alice@Example.com".to_string()], &[], &[], &allowed)
            .unwrap();
    }

    #[test]
    fn reject_disallowed_recipients_refuses_empty_allowlist() {
        let err = reject_disallowed_recipients(&["alice@example.com".to_string()], &[], &[], &[])
            .unwrap_err();
        assert!(matches!(err, AgentError::SandboxDenied(_)));
    }
}
