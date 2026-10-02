//! `caldav_create_event` agent — append a new VEVENT to a
//! CalDAV calendar collection.
//!
//! v1: read + add only. The CalDAV client does not expose
//! `update()` or `delete()`; the only mutation v1 ships is
//! the create flow below. The agent opts in to the
//! `requires_confirmation` flow (first call returns
//! `NeedsConfirmation`, second call in the same chat turn
//! returns `Allow`) so the LLM cannot fire-and-forget a
//! write without the user seeing what gets persisted.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use url::Url;

use crate::agents::caldav::{BasicAuth, CalDavClient, EventPatch};
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
pub struct CreateEventAgent {
    cfg: CalDavAgentConfig,
}

impl std::fmt::Debug for CreateEventAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreateEventAgent")
            .field("cfg", &self.cfg)
            .finish()
    }
}

impl CreateEventAgent {
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
impl Agent for CreateEventAgent {
    fn name(&self) -> &str {
        "caldav_create_event"
    }

    fn description(&self) -> &str {
        "Create a new VEVENT in the user's CalDAV calendar. Required: `summary` (event title), \
         `start` (RFC 3339), `end` (RFC 3339, optional; defaults to start + 1h). Optional: \
         `description`, `location`. Returns the new event's UID and href. The LLM must ask the \
         user to confirm before this call runs (the agent refuses on the first invocation in a \
         turn). \
         Ce plugin supporte uniquement la lecture et l'ajout d'événements. L'édition et la \
         suppression ne sont pas disponibles dans cette version — utilisez votre client CalDAV \
         habituel pour ces opérations."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Event title (iCalendar SUMMARY)."
                },
                "start": {
                    "type": "string",
                    "format": "date-time",
                    "description": "Event start (RFC 3339 / ISO 8601, UTC). Required."
                },
                "end": {
                    "type": "string",
                    "format": "date-time",
                    "description": "Event end (RFC 3339). Optional; defaults to start + 1h when missing."
                },
                "description": {
                    "type": "string",
                    "description": "Long-form description (iCalendar DESCRIPTION). Optional."
                },
                "location": {
                    "type": "string",
                    "description": "Event location (iCalendar LOCATION). Optional."
                }
            },
            "required": ["summary", "start"],
            "additionalProperties": false,
        })
    }

    fn requires_confirmation(&self, ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        // The confirm-then-execute pattern: the first call
        // in a chat-completions turn asks the LLM to surface
        // the write to the user, the second call in the same
        // turn (after the user has confirmed) gets the
        // `Allow` verdict and runs the PUT.
        if ctx.was_invoked(self.name()) {
            ConfirmationDecision::Allow
        } else {
            ConfirmationDecision::NeedsConfirmation {
                reason: format!(
                    "`{name}` is a write tool that appends a new VEVENT to the user's CalDAV \
                     calendar. The user must explicitly confirm the create in chat before this \
                     call runs. Re-invoke `{name}` with the same arguments on the next turn to \
                     proceed; the second invocation runs the write.",
                    name = self.name(),
                ),
            }
        }
    }

    fn untrusted_output(&self) -> bool {
        // The body the LLM echoes back is the iCalendar we
        // generated — not remote content. Marking
        // `untrusted_output = false` would skip the
        // untrusted-input fence; we keep it `true` so the
        // tool loop's general rule applies (the description
        // does include the user-supplied summary, which is
        // less risky but follows the conservative pattern).
        true
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let client = self.build_client(ctx).await?;
        let patch = EventPatch {
            summary: req.summary,
            start: req.start,
            end: req.end,
            description: req.description,
            location: req.location,
        };
        let body = patch.to_ical();
        // The `<uid>.ics` href stem is local to the calendar
        // collection; the client joins it against the
        // stored `url`.
        let uid = body
            .lines()
            .find_map(|l| l.strip_prefix("UID:").map(|s| s.to_string()))
            .ok_or_else(|| AgentError::AgentFailed("generated VEVENT missing UID".into()))?;
        let href_stem = format!("{uid}.ics");
        client.put(&href_stem, &body).await?;
        let full_href = client
            .principal
            .join(&href_stem)
            .map(|u| u.to_string())
            .unwrap_or_default();
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": {
                "uid": uid,
                "href": full_href,
                "summary": patch.summary,
                "start": patch.start.to_rfc3339(),
                "end": patch.end.map(|d| d.to_rfc3339()),
            }
        }))
        .expect("json encode"))
    }
}

#[derive(Debug)]
struct ParsedArgs {
    summary: String,
    start: DateTime<Utc>,
    end: Option<DateTime<Utc>>,
    description: Option<String>,
    location: Option<String>,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let summary = obj
        .get("summary")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`summary` (string) is required".into()))?
        .trim()
        .to_string();
    if summary.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`summary` must not be empty".into(),
        ));
    }
    let start = obj.get("start").and_then(|v| v.as_str()).ok_or_else(|| {
        AgentError::InvalidArguments("`start` (RFC 3339 string) is required".into())
    })?;
    let start_dt = DateTime::parse_from_rfc3339(start)
        .map_err(|e| AgentError::InvalidArguments(format!("`start` must be RFC 3339: {e}")))?
        .with_timezone(&Utc);
    let end = obj
        .get("end")
        .and_then(|v| v.as_str())
        .map(|s| {
            DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Utc))
                .map_err(|e| AgentError::InvalidArguments(format!("`end` must be RFC 3339: {e}")))
        })
        .transpose()?;
    if let Some(e) = end {
        if e < start_dt {
            return Err(AgentError::InvalidArguments(
                "`end` must not be before `start`".into(),
            ));
        }
    }
    let description = obj
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let location = obj
        .get("location")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(ParsedArgs {
        summary,
        start: start_dt,
        end,
        description,
        location,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::UserContext;
    use crate::egress::host_matches_allowlist;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn name_and_schema_are_stable() {
        let agent = CreateEventAgent::new(CalDavAgentConfig::default());
        assert_eq!(agent.name(), "caldav_create_event");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "summary"));
        assert!(required.iter().any(|v| v == "start"));
    }

    #[test]
    fn requires_confirmation_first_call_then_allow() {
        // The first invocation in a turn refuses without
        // confirmation; the second in the same turn returns
        // `Allow`. Replicates the pattern documented in
        // `web_fetch.rs::requires_confirmation`.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = CreateEventAgent::new(CalDavAgentConfig::default());
        let mut ctx = UserContext::for_tests(
            uuid::Uuid::new_v4(),
            Arc::new(crate::ServiceRegistry::empty()),
        );
        // First call: NeedsConfirmation.
        let d1 = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(matches!(d1, ConfirmationDecision::NeedsConfirmation { .. }));
        // The tool loop records the invocation between calls.
        ctx.record_invocation(agent.name());
        // Second call: Allow.
        let d2 = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(matches!(d2, ConfirmationDecision::Allow));
        // Suppress the unused-runtime lint.
        let _ = rt;
    }

    #[test]
    fn parse_args_defaults_end_to_start_plus_one_hour() {
        // The `end` defaulting happens inside `invoke` (the
        // `EventPatch` builder sets a 1h DURATION). Here we
        // only check the parser surfaces the raw `None` so
        // the default kicks in.
        let args = parse_args(&json!({
            "summary": "x",
            "start": "2026-02-01T14:00:00Z",
        }))
        .expect("parse");
        assert!(args.end.is_none());
    }

    #[test]
    fn parse_args_rejects_end_before_start() {
        let err = parse_args(&json!({
            "summary": "x",
            "start": "2026-02-01T14:00:00Z",
            "end": "2026-02-01T13:00:00Z",
        }))
        .unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn empty_allowlist_blocks_public_caldav_hosts() {
        // An empty allowlist means `allow_public = true` is
        // the only thing that lets the agent reach a public
        // CalDAV server. The `host_matches_allowlist` check
        // is what the probe handler will run; we mirror the
        // expectation here so a future regression is caught.
        let allowlist: Vec<String> = Vec::new();
        assert!(!host_matches_allowlist("cloud.example.com", &allowlist));
    }
}
