//! `email_search` agent — run a constrained IMAP `SEARCH` and
//! return matching summaries. Plan 1791566269874 Phase 4.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::email::parse::validate_search_criteria;
use crate::agents::email::{EmailImap, ImapConnectConfig};
use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};
use crate::egress::host_matches_allowlist;
use crate::EmailAgentConfig;

use super::list_inbox::build_imap_config;

#[derive(Clone)]
pub struct SearchAgent {
    cfg: std::sync::Arc<EmailAgentConfig>,
    imap: Option<std::sync::Arc<dyn EmailImap>>,
    allowlist: Vec<String>,
}

impl std::fmt::Debug for SearchAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchAgent")
            .field("cfg", &self.cfg)
            .field("imap", &self.imap.as_ref().map(|_| "<dyn EmailImap>"))
            .field("allowlist", &self.allowlist)
            .finish()
    }
}

impl SearchAgent {
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
impl Agent for SearchAgent {
    fn name(&self) -> &str {
        "email_search"
    }

    fn description(&self) -> &str {
        "Run a constrained IMAP `SEARCH` against the user's mailbox. `query` is restricted to a \
         small allow-list of IMAP criteria tokens: `ALL`, `UNSEEN`, `SEEN`, `SUBJECT <text>`, \
         `FROM <text>`, `TO <text>`, `BODY <text>`, `SINCE <date>`, `BEFORE <date>`, \
         `UID <set>`. Free-form Gmail-style queries (`from:alice subject:report`) are \
         deliberately v2 — see plan 1791566269874 §10 Q4. Returns the matching message \
         summaries, capped at `limit` (default 25, cap 100). Read operations on a third-party \
         service with the user's credentials are gated by the inline approval card."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "IMAP SEARCH criteria string. Restricted to the v1 token allow-list — see the agent description."
                },
                "folder": {
                    "type": "string",
                    "description": "Mailbox folder. Default `INBOX`."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "description": "Maximum number of summaries to return. Default 25, cap 100."
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        // Synonyms the BM25 router pre-selection keys on
        // for the natural-language query "search emails"
        // / "find a message from X" / "look up an email".
        // Without these the router returns
        // `get_weather` (which has the word "days" in its
        // forecast-API prose) as the top hit for "list
        // emails from the last 2 days" — a real bug
        // the user hit. See the comment on
        // `list_inbox::keywords` for the full rationale.
        &[
            "email", "emails", "mail", "inbox", "messages", "search", "find", "look", "lookup",
            "query", "filter", "from", "subject", "imap",
        ]
    }

    fn requires_confirmation(&self, ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        if ctx.was_invoked(self.name()) {
            ConfirmationDecision::Allow
        } else {
            ConfirmationDecision::NeedsConfirmation {
                reason: format!(
                    "`{name}` runs an IMAP SEARCH against the user's mailbox. The matched \
                     subjects and senders are sensitive. The user must explicitly confirm the \
                     read in chat before this call runs. Re-invoke `{name}` with the same \
                     arguments on the next turn to proceed; the second invocation runs the \
                     search.",
                    name = self.name(),
                ),
            }
        }
    }

    fn untrusted_output(&self) -> bool {
        true
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args, self.cfg.max_messages_per_call)?;
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
        let summaries = imap
            .search(ctx.user_id(), &config, &req.folder, &req.query, req.limit)
            .await?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": {
                "messages": summaries,
                "folder": req.folder,
                "count": summaries.len(),
                "query": req.query,
            }
        }))
        .expect("json encode"))
    }
}

#[derive(Debug)]
struct ParsedArgs {
    query: String,
    folder: String,
    limit: usize,
}

fn parse_args(args: &Value, cap: usize) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let query_raw = obj
        .get("query")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`query` (string) is required".into()))?;
    let query = validate_search_criteria(query_raw)?;
    let folder = obj
        .get("folder")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "INBOX".to_string());
    let limit = obj
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(25)
        .min(cap)
        .max(1);
    Ok(ParsedArgs {
        query,
        folder,
        limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServiceRegistry;
    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn requires_confirmation_first_call_then_allow() {
        let agent = SearchAgent::new(Arc::new(EmailAgentConfig::default()), None, Vec::new());
        let mut ctx = UserContext::for_tests(Uuid::new_v4(), Arc::new(ServiceRegistry::empty()));
        let d1 = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(matches!(d1, ConfirmationDecision::NeedsConfirmation { .. }));
        ctx.record_invocation(agent.name());
        let d2 = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(matches!(d2, ConfirmationDecision::Allow));
    }

    #[test]
    fn parse_args_rejects_missing_query() {
        let err = parse_args(&json!({}), 100).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_unknown_keyword() {
        // v1 only allows a small token set.
        let err = parse_args(&json!({"query": "OR FROM a FROM b"}), 100).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_accepts_known_query() {
        let args = parse_args(&json!({"query": "FROM alice"}), 100).unwrap();
        assert_eq!(args.query, "from \"alice\"");
    }
}
