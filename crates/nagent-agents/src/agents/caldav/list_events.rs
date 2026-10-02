//! `caldav_list_events` agent — list VEVENTs in a calendar
//! between two timestamps.
//!
//! v1: read + add only. No edit / delete tools. The
//! per-user calendar collection URL is stored in the
//! per-user vault under the `caldav` service id (`url`,
//! `username`, `password`).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use url::Url;

use crate::agents::caldav::{
    build_calendar_query_body, extract_events_from_multistatus, BasicAuth, CalDavClient,
};
use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};
use crate::egress::EgressClient;
use crate::CalDavAgentConfig;

/// Build the per-call `EgressClient` for a CalDAV request from
/// the agent config. The `allowlist` enforces the operator's
/// host boundary; the timeout is the per-request cap.
fn build_egress(cfg: &CalDavAgentConfig) -> EgressClient {
    EgressClient::new(crate::egress::EgressConfig {
        timeout_ms: cfg.timeout_ms,
        // Public hosts (Nextcloud, Fastmail, iCloud, …) need
        // `allow_public = true`; the allowlist (when set)
        // narrows the set.
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
pub struct ListEventsAgent {
    cfg: CalDavAgentConfig,
}

impl std::fmt::Debug for ListEventsAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListEventsAgent")
            .field("cfg", &self.cfg)
            .finish()
    }
}

impl ListEventsAgent {
    pub fn new(cfg: CalDavAgentConfig) -> Self {
        Self { cfg }
    }

    async fn build_client(
        &self,
        ctx: &UserContext,
        calendar_url: &str,
    ) -> Result<CalDavClient, AgentError> {
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
        // Cross-check the LLM-supplied calendar_url: when set,
        // it must point at the same server (same scheme + host)
        // as the stored `url` so a misconfigured vault cannot
        // be coerced into reaching a third-party host.
        if !calendar_url.is_empty() {
            let llm = Url::parse(calendar_url)
                .map_err(|e| AgentError::InvalidArguments(format!("calendar_url: {e}")))?;
            if llm.scheme() != principal.scheme() || llm.host_str() != principal.host_str() {
                return Err(AgentError::InvalidArguments(
                    "calendar_url host does not match the configured CalDAV server".into(),
                ));
            }
        }
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
impl Agent for ListEventsAgent {
    fn name(&self) -> &str {
        "caldav_list_events"
    }

    fn description(&self) -> &str {
        "List events from the user's CalDAV calendar in a time range. Returns JSON with `events[]` \
         (uid, summary, start, end, description?, location?, rrule?). Accepts RFC 3339 `start` and \
         `end` timestamps and an optional `calendar_url` override (defaults to the one stored in \
         the user's CalDAV connector config). \
         Ce plugin supporte uniquement la lecture et l'ajout d'événements. L'édition et la \
         suppression ne sont pas disponibles dans cette version — utilisez votre client CalDAV \
         habituel pour ces opérations."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "start": {
                    "type": "string",
                    "format": "date-time",
                    "description": "Inclusive start of the time range (RFC 3339 / ISO 8601, UTC). Required."
                },
                "end": {
                    "type": "string",
                    "format": "date-time",
                    "description": "Exclusive end of the time range (RFC 3339, UTC). Required."
                },
                "calendar_url": {
                    "type": "string",
                    "description": "Optional override for the calendar collection URL. Must point at the same CalDAV server as the stored `url`. Defaults to the saved `url`."
                },
                "max_events": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 1000,
                    "description": "Override the server-wide cap on returned events for this call."
                }
            },
            "required": ["start", "end"],
            "additionalProperties": false,
        })
    }

    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        // Read-only tool: no extra user confirmation.
        ConfirmationDecision::Allow
    }

    fn untrusted_output(&self) -> bool {
        // Event summaries + descriptions come from a remote
        // CalDAV server the LLM does not own. The
        // untrusted-input fence in the tool loop wraps the
        // result before the next round.
        true
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let client = self.build_client(ctx, &req.calendar_url).await?;
        let calendar_url = if req.calendar_url.is_empty() {
            // Use the stored `url` directly (it's the calendar
            // collection the user picked during setup).
            client.principal.as_str().to_string()
        } else {
            req.calendar_url.clone()
        };
        let body = build_calendar_query_body(req.start, req.end);
        let bytes = client.report(&calendar_url, &body).await?;
        let mut events = extract_events_from_multistatus(
            std::str::from_utf8(&bytes).unwrap_or(""),
            &client.principal,
        )?;
        // Apply the cap.
        let cap = req.max_events.unwrap_or(self.cfg.max_events);
        if events.len() > cap {
            events.truncate(cap);
        }
        // Project to the LLM-facing JSON (drop `href`; the
        // LLM uses `uid` as the stable identifier).
        let projected: Vec<Value> = events
            .iter()
            .map(|e| {
                json!({
                    "uid": e.uid,
                    "summary": e.summary,
                    "start": e.dt_start.to_rfc3339(),
                    "end": e.dt_end.map(|d| d.to_rfc3339()),
                    "description": e.description,
                    "location": e.location,
                    "rrule": e.rrule,
                })
            })
            .collect();
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": {
                "events": projected,
                "count": projected.len(),
                "start": req.start.to_rfc3339(),
                "end": req.end.to_rfc3339(),
            }
        }))
        .expect("json encode"))
    }
}

#[derive(Debug)]
struct ParsedArgs {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    calendar_url: String,
    max_events: Option<usize>,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let start = obj.get("start").and_then(|v| v.as_str()).ok_or_else(|| {
        AgentError::InvalidArguments("`start` (RFC 3339 string) is required".into())
    })?;
    let end = obj.get("end").and_then(|v| v.as_str()).ok_or_else(|| {
        AgentError::InvalidArguments("`end` (RFC 3339 string) is required".into())
    })?;
    let start_dt = DateTime::parse_from_rfc3339(start)
        .map_err(|e| AgentError::InvalidArguments(format!("`start` must be RFC 3339: {e}")))?
        .with_timezone(&Utc);
    let end_dt = DateTime::parse_from_rfc3339(end)
        .map_err(|e| AgentError::InvalidArguments(format!("`end` must be RFC 3339: {e}")))?
        .with_timezone(&Utc);
    if end_dt <= start_dt {
        return Err(AgentError::InvalidArguments(
            "`end` must be after `start`".into(),
        ));
    }
    let calendar_url = obj
        .get("calendar_url")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    let max_events = obj
        .get("max_events")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    Ok(ParsedArgs {
        start: start_dt,
        end: end_dt,
        calendar_url,
        max_events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_schema_are_stable() {
        let agent = ListEventsAgent::new(CalDavAgentConfig::default());
        assert_eq!(agent.name(), "caldav_list_events");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "start"));
        assert!(required.iter().any(|v| v == "end"));
        assert!(schema["properties"]["calendar_url"]["type"].is_string());
        assert_eq!(schema["properties"]["max_events"]["maximum"], 1000);
    }

    #[test]
    fn description_warns_about_no_edit_or_delete() {
        let agent = ListEventsAgent::new(CalDavAgentConfig::default());
        let desc = agent.description();
        assert!(desc.contains("lecture et l'ajout"));
        assert!(desc.contains("L'édition et la suppression"));
    }

    #[test]
    fn parse_args_rejects_bad_range() {
        let err = parse_args(&json!({
            "start": "2026-02-01T00:00:00Z",
            "end": "2026-01-01T00:00:00Z",
        }))
        .unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_missing_end() {
        let err = parse_args(&json!({"start": "2026-02-01T00:00:00Z"})).unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    /// Cross-user isolation (plan §3, AGENTS.md §3): the
    /// `CalDavClient` is built per-call from `ctx.secret(...)`,
    /// never from any shared global state. A test-only
    /// assertion that the constructor is the only way the
    /// agent can reach the CalDAV server: a stolen
    /// `UserContext` from another user cannot see / reach the
    /// first user's events because the per-user vault is
    /// keyed on `ctx.user_id()`.
    ///
    /// The actual proof requires a live `SecretSource` mock;
    /// we exercise the path that builds the client and
    /// assert the failure surface when no resolver is wired
    /// (the test ctx has `resolver: None`).
    #[test]
    fn missing_resolver_surfaces_credentials_missing_error() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = ListEventsAgent::new(CalDavAgentConfig::default());
        let ctx = UserContext::for_tests(
            uuid::Uuid::new_v4(),
            std::sync::Arc::new(crate::ServiceRegistry::empty()),
        );
        let err = rt
            .block_on(agent.invoke(
                &ctx,
                json!({
                    "start": "2026-02-01T00:00:00Z",
                    "end": "2026-02-02T00:00:00Z",
                }),
            ))
            .unwrap_err();
        // `for_tests` has no resolver, so `ctx.secret(...)`
        // returns `CredentialsMissing`. This guards the
        // cross-user path: an attacker who got a `UserContext`
        // for a user with no resolver sees the same error as a
        // legitimate caller with an unconfigured vault.
        assert!(matches!(err, AgentError::CredentialsMissing { .. }));
    }
}
