//! `caldav_get_event` agent — fetch a single VEVENT by UID.
//!
//! The agent issues a `calendar-query` REPORT narrowed to the
//! UID, then returns the first matching `VEVENT`. The
//! per-user calendar collection URL is stored in the
//! per-user vault under the `caldav` service id.
//!
//! v1: read + add only. No edit / delete tools.

use async_trait::async_trait;
use serde_json::{json, Value};
use url::Url;

use crate::agents::caldav::{
    build_calendar_query_body, extract_events_from_multistatus, parse_vevent, BasicAuth,
    CalDavClient,
};
use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};
use crate::egress::EgressClient;
use crate::CalDavAgentConfig;

fn build_egress(cfg: &CalDavAgentConfig) -> EgressClient {
    EgressClient::new(crate::egress::EgressConfig {
        timeout_ms: cfg.timeout_ms,
        allow_public: true,
        allowlist: cfg.allowlist.clone(),
        max_bytes: cfg.max_body_bytes,
        user_agent: format!(
            "nagent-caldav-agent/{} (+https://github.com/nagent/nagent)",
            env!("CARGO_PKG_VERSION")
        ),
    })
}

#[derive(Clone)]
pub struct GetEventAgent {
    cfg: CalDavAgentConfig,
}

impl std::fmt::Debug for GetEventAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GetEventAgent")
            .field("cfg", &self.cfg)
            .finish()
    }
}

impl GetEventAgent {
    pub fn new(cfg: CalDavAgentConfig) -> Self {
        Self { cfg }
    }

    async fn build_client(&self, ctx: &UserContext) -> Result<CalDavClient, AgentError> {
        let url_secret = ctx
            .secret("caldav", "url")
            .await?
            .ok_or_else(|| missing_field("url"))?;
        let username = ctx
            .secret("caldav", "username")
            .await?
            .ok_or_else(|| missing_field("username"))?;
        let password = ctx
            .secret("caldav", "password")
            .await?
            .ok_or_else(|| missing_field("password"))?;
        use secrecy::ExposeSecret;
        let principal = Url::parse(url_secret.expose_secret())
            .map_err(|e| AgentError::InvalidArguments(format!("stored caldav url: {e}")))?;
        Ok(CalDavClient::new(
            build_egress(&self.cfg),
            principal,
            BasicAuth {
                username: username.expose_secret().to_string(),
                password: password.expose_secret().to_string(),
            },
        ))
    }
}

fn missing_field(field: &str) -> AgentError {
    AgentError::AgentFailed(format!(
        "caldav credentials are not configured: set the `{field}` field in the Integrations page"
    ))
}

#[async_trait]
impl Agent for GetEventAgent {
    fn name(&self) -> &str {
        "caldav_get_event"
    }

    fn description(&self) -> &str {
        "Fetch a single CalDAV event by its UID. Returns the full VEVENT (summary, start, end, \
         description, location, rrule). Pass `uid` (the value returned by `caldav_list_events`). \
         The LLM must ask the user to confirm before this call runs (the agent refuses on the \
         first invocation in a turn) — calendar contents are sensitive. Re-invoke \
         `caldav_get_event` with the same arguments on the next turn to proceed; the second \
         invocation runs the read. \
         Ce plugin supporte uniquement la lecture et l'ajout d'événements. L'édition et la \
         suppression ne sont pas disponibles dans cette version — utilisez votre client CalDAV \
         habituel pour ces opérations."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "uid": {
                    "type": "string",
                    "description": "The VEVENT UID returned by `caldav_list_events`."
                }
            },
            "required": ["uid"],
            "additionalProperties": false,
        })
    }

    fn requires_confirmation(&self, ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        // Same pattern as `caldav_list_events` / `caldav_create_event`:
        // first invocation refuses with a reason the LLM relays
        // to the user; the second invocation (after the user has
        // confirmed) runs the read.
        if ctx.was_invoked(self.name()) {
            ConfirmationDecision::Allow
        } else {
            ConfirmationDecision::NeedsConfirmation {
                reason: format!(
                    "`{name}` reads a single event from the user's CalDAV calendar. Calendar \
                     contents are sensitive. The user must explicitly confirm the read in chat \
                     before this call runs. Re-invoke `{name}` with the same arguments on the \
                     next turn to proceed; the second invocation runs the read.",
                    name = self.name(),
                ),
            }
        }
    }

    fn untrusted_output(&self) -> bool {
        true
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let client = self.build_client(ctx).await?;
        // The CalDAV server typically does not support
        // UID-scoped queries, so we run a broad 1-year REPORT
        // and filter client-side. The cap on the parsed
        // events (max_events) is a guard against a runaway
        // calendar.
        let now = chrono::Utc::now();
        let start = now - chrono::Duration::days(366);
        let end = now + chrono::Duration::days(366);
        let body = build_calendar_query_body(start, end);
        let bytes = client.report(client.principal.as_str(), &body).await?;
        let events = extract_events_from_multistatus(
            std::str::from_utf8(&bytes).unwrap_or(""),
            &client.principal,
        )?;
        // Match the UID; if found, return the event.
        let matched = events.into_iter().find(|e| e.uid == req.uid);
        if let Some(e) = matched {
            return Ok(serde_json::to_string(&json!({
                "ok": true,
                "data": {
                    "uid": e.uid,
                    "summary": e.summary,
                    "start": e.dt_start.to_rfc3339(),
                    "end": e.dt_end.map(|d| d.to_rfc3339()),
                    "description": e.description,
                    "location": e.location,
                    "rrule": e.rrule,
                }
            }))
            .expect("json encode"));
        }
        // Fall back to direct fetch: the server may have
        // served a 207 with no matching VEVENT because the
        // time range missed it. Try the calendar root with
        // the UID as the href stem.
        let href = format!("{}.ics", req.uid);
        let full = client
            .principal
            .join(&href)
            .map_err(|e| AgentError::InvalidArguments(format!("href join: {e}")))?;
        match client.get(full.as_str()).await {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes).map_err(|e| {
                    AgentError::AgentFailed(format!("iCalendar body not utf-8: {e}"))
                })?;
                let event = parse_vevent(text, full.as_str())?;
                Ok(serde_json::to_string(&json!({
                    "ok": true,
                    "data": {
                        "uid": event.uid,
                        "summary": event.summary,
                        "start": event.dt_start.to_rfc3339(),
                        "end": event.dt_end.map(|d| d.to_rfc3339()),
                        "description": event.description,
                        "location": event.location,
                        "rrule": event.rrule,
                    }
                }))
                .expect("json encode"))
            }
            Err(AgentError::Upstream { status: 404, .. }) => Err(AgentError::AgentFailed(format!(
                "no event with uid `{}` in the configured calendar",
                req.uid
            ))),
            Err(other) => Err(other),
        }
    }
}

#[derive(Debug)]
struct ParsedArgs {
    uid: String,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let uid = obj
        .get("uid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`uid` (string) is required".into()))?
        .trim()
        .to_string();
    if uid.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`uid` must not be empty".into(),
        ));
    }
    Ok(ParsedArgs { uid })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::UserContext;
    use std::sync::Arc;

    #[test]
    fn requires_confirmation_first_call_then_allow() {
        // Mirrors `caldav_list_events`: even single-event reads
        // gate on confirmation so the user is in control.
        let agent = GetEventAgent::new(CalDavAgentConfig::default());
        let mut ctx = UserContext::for_tests(
            uuid::Uuid::new_v4(),
            Arc::new(crate::ServiceRegistry::empty()),
        );
        let d1 = agent.requires_confirmation(&ctx, &json!({"uid": "x"}));
        assert!(
            matches!(d1, ConfirmationDecision::NeedsConfirmation { .. }),
            "first call must require confirmation; got {d1:?}"
        );
        ctx.record_invocation(agent.name());
        let d2 = agent.requires_confirmation(&ctx, &json!({"uid": "x"}));
        assert!(
            matches!(d2, ConfirmationDecision::Allow),
            "second call in the same turn must be `Allow`; got {d2:?}"
        );
    }

    #[test]
    fn name_and_schema_are_stable() {
        let agent = GetEventAgent::new(CalDavAgentConfig::default());
        assert_eq!(agent.name(), "caldav_get_event");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "uid"));
    }

    #[test]
    fn parse_args_rejects_empty_uid() {
        let err = parse_args(&json!({"uid": "  "})).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }
}
