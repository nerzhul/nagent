//! `email_list_inbox` agent — list the most recent messages in a
//! folder (default `INBOX`).
//!
//! v1 per-call connection model: every invocation opens a fresh
//! IMAP connection, runs `SELECT` + `SEARCH ALL` + `FETCH
//! ENVELOPE`, and closes. See `super::client_imap` for the wire
//! layer (Phase 3 follow-up to plan 1791566269874).

use async_trait::async_trait;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::agents::email::{EmailImap, ImapConnectConfig, ImapTlsMode};
use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};
use crate::egress::host_matches_allowlist;
use crate::EmailAgentConfig;

/// `email_list_inbox` agent. Read-only; the LLM-facing JSON
/// surfaces a list of `EmailSummary` rows capped at
/// `max_messages_per_call`.
#[derive(Clone)]
pub struct ListInboxAgent {
    cfg: std::sync::Arc<EmailAgentConfig>,
    /// The shared `EmailImap` capability. `None` for test
    /// contexts that never reach the wire (the `for_tests()`
    /// path on `UserContext`).
    imap: Option<std::sync::Arc<dyn EmailImap>>,
    /// Operator hostname allow-list. The default is empty
    /// (fail-closed); the runtime config supplies the
    /// per-deployment set.
    allowlist: Vec<String>,
}

impl std::fmt::Debug for ListInboxAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListInboxAgent")
            .field("cfg", &self.cfg)
            .field("imap", &self.imap.as_ref().map(|_| "<dyn EmailImap>"))
            .field("allowlist", &self.allowlist)
            .finish()
    }
}

impl ListInboxAgent {
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
impl Agent for ListInboxAgent {
    fn name(&self) -> &str {
        "email_list_inbox"
    }

    fn description(&self) -> &str {
        "List the most recent messages in a folder of the user's IMAP mailbox (default `INBOX`). \
         Returns JSON with `messages[]` (uid, folder, from, subject, date, snippet, seen) capped \
         at the server-side limit. Accepts an optional `folder` (default `INBOX`), an optional \
         RFC 3339 `since`, and an optional `limit` (default 25, cap 100). Read operations on a \
         third-party service with the user's credentials are gated by the inline approval card \
         — the first invocation in a turn requires user confirmation, the second runs. The IMAP \
         wire client connects per call (no persistent state on the server)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "folder": {
                    "type": "string",
                    "description": "Mailbox folder to list. Default `INBOX`.",
                },
                "since": {
                    "type": "string",
                    "format": "date-time",
                    "description": "RFC 3339 lower bound on `Date:` header. Optional.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Maximum number of summaries to return. Default 25, cap 100.",
                },
            },
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        // Synonyms the BM25 router pre-selection keys on
        // for the natural-language query "list emails"
        // / "show my inbox" / "what's new in my email".
        // The description has the technical detail
        // (IMAP, mailbox, folder, …); the keywords add
        // the user-facing vocabulary that is missing
        // from the description's prose. Without these
        // the router returns `get_weather` (which has
        // the word "days" in its forecast-API prose) as
        // the top hit for "list emails from the last 2
        // days" — a real bug the user hit.
        &[
            "email", "emails", "mail", "inbox", "messages", "list", "recent", "show", "check",
            "read", "imap",
        ]
    }

    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        // Inbox contents are sensitive: even read access hits a
        // third-party service with the user's credentials. Plan
        // 1791834996882 closes the same-turn re-call shortcut —
        // every call must require a fresh user click. The
        // session-override button on the approval card remains the
        // only click-skip path; that bypass lives in the tool
        // loop's `force_allow` check, not here.
        ConfirmationDecision::NeedsConfirmation {
            reason: format!(
                "`{name}` reads the user's IMAP mailbox. Inbox contents are sensitive \
                 (sender, subject, snippet). The user must explicitly confirm the read \
                 in chat before this call runs. Wait for the user to click \"Autoriser\" \
                 in the chat UI; the tool will execute on their next message and you \
                 will see its result then.",
                name = self.name(),
            ),
        }
    }

    fn untrusted_output(&self) -> bool {
        // Senders + subjects come from a remote IMAP server
        // the LLM does not own; the untrusted-input fence
        // wraps the result before the next round.
        true
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args, self.cfg.max_messages_per_call)?;
        let config = build_imap_config(ctx).await?;
        // Hostname allow-list check — fail-closed when empty.
        if self.allowlist.is_empty() || !host_matches_allowlist(&config.host, &self.allowlist) {
            return Err(AgentError::SandboxDenied(format!(
                "host `{}` is not in the email allowlist (set `EMAIL_ALLOWLIST` on the server)",
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
        let summaries = imap
            .list_inbox(ctx.user_id(), &config, &req.folder, req.since, req.limit)
            .await?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": {
                "messages": summaries,
                "folder": req.folder,
                "count": summaries.len(),
            }
        }))
        .expect("json encode"))
    }
}

#[derive(Debug)]
struct ParsedArgs {
    folder: String,
    since: Option<chrono::DateTime<chrono::Utc>>,
    limit: usize,
}

fn parse_args(args: &Value, cap: usize) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let folder = obj
        .get("folder")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "INBOX".to_string());
    let since = obj
        .get("since")
        .and_then(|v| v.as_str())
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|d| d.with_timezone(&chrono::Utc))
                .map_err(|e| AgentError::InvalidArguments(format!("`since` must be RFC 3339: {e}")))
        })
        .transpose()?;
    let limit = obj
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(25)
        .min(cap)
        .max(1);
    Ok(ParsedArgs {
        folder,
        since,
        limit,
    })
}

/// Build the per-call `ImapConnectConfig` from the per-user vault.
/// Returns the field-by-field error so a misconfigured vault
/// surfaces a clear message instead of `CredentialsMissing` for
/// every missing field.
pub async fn build_imap_config(ctx: &UserContext) -> Result<ImapConnectConfig, AgentError> {
    let host = ctx
        .secret("email_imap", "host")
        .await?
        .ok_or_else(|| missing_field("host"))?;
    let port = ctx
        .secret("email_imap", "port")
        .await?
        .ok_or_else(|| missing_field("port"))?;
    let tls_mode = ctx
        .secret("email_imap", "tls_mode")
        .await?
        .ok_or_else(|| missing_field("tls_mode"))?;
    let username = ctx
        .secret("email_imap", "username")
        .await?
        .ok_or_else(|| missing_field("username"))?;
    // The `password` is needed for the wire client. We
    // pull it from the vault here so the `EmailImapImpl`
    // can `LOGIN` without a second round-trip to the
    // secret resolver; the per-user secret stays in
    // memory only for the duration of the call.
    let password = ctx
        .secret("email_imap", "password")
        .await?
        .ok_or_else(|| missing_field("password"))?;
    use secrecy::ExposeSecret;
    let host_s = host.expose_secret().to_string();
    let port_s = port.expose_secret().trim().to_string();
    let port_num: u16 = port_s.parse().map_err(|_| {
        AgentError::InvalidArguments(format!(
            "email_imap.port must be an integer (got `{port_s}`)"
        ))
    })?;
    let tls_mode = ImapTlsMode::parse(tls_mode.expose_secret())?;
    Ok(ImapConnectConfig {
        host: host_s,
        port: port_num,
        tls_mode,
        username: username.expose_secret().to_string(),
        password: password.expose_secret().to_string(),
    })
}

fn missing_field(field: &str) -> AgentError {
    AgentError::AgentFailed(format!(
        "email_imap credentials are not configured: set the `{field}` field in the Integrations page"
    ))
}

// Allow the test helper to reach `Uuid` for cross-user mocks.
#[allow(dead_code)]
fn _uuid_marker(_: Uuid) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceRegistry;
    use std::sync::Arc;

    #[test]
    fn requires_confirmation_is_strict_even_in_same_turn() {
        // Plan 1791834996882: same-turn re-call shortcut is
        // closed — both calls must require confirmation.
        let agent = ListInboxAgent::new(Arc::new(EmailAgentConfig::default()), None, Vec::new());
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
    fn parse_args_defaults_folder_to_inbox() {
        let args = parse_args(&json!({}), 100).unwrap();
        assert_eq!(args.folder, "INBOX");
        assert_eq!(args.limit, 25);
    }

    #[test]
    fn parse_args_clamps_limit_to_cap() {
        let args = parse_args(&json!({"limit": 9999}), 100).unwrap();
        assert_eq!(args.limit, 100);
    }

    #[test]
    fn parse_args_rejects_bad_since() {
        let err = parse_args(&json!({"since": "yesterday"}), 100).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }
}
