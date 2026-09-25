//! `get_datetime` agent: returns the current wall-clock time, optionally
//! localised to an IANA timezone.
//!
//! The agent is purely local — no HTTP, no I/O other than reading the
//! system clock. It exists so the LLM can answer "quelle heure est-il à
//! Tokyo ?" without falling back to its training data, and so the chat
//! experience feels grounded when the user asks "date du jour" or
//! "what time is it in New York?".
//!
//! ## IANA timezone handling
//!
//! The `chrono-tz` crate ships its own IANA bundle (via the bundled
//! `tz-data` feature). This is the right default: many minimal Docker
//! images do not carry `/usr/share/zoneinfo`, and "use system tzdata"
//! would then fail at runtime. Operators who need to shave binary size
//! can switch to system tzdata in the workspace `Cargo.toml` by
//! removing the bundled feature.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError};

/// Build of the `get_datetime` agent. Stateless — the constructor
/// only stores the `chrono-tz` resolution table, which is itself
/// stateless.
#[derive(Clone)]
pub struct DateTimeAgent;

impl std::fmt::Debug for DateTimeAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DateTimeAgent").finish()
    }
}

impl Default for DateTimeAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl DateTimeAgent {
    /// Construct a new agent. There is no configuration knob in v1 —
    /// the agent is a thin wrapper over `chrono::Utc::now()` and the
    /// bundled IANA timezone database.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Agent for DateTimeAgent {
    fn name(&self) -> &str {
        "get_datetime"
    }

    fn description(&self) -> &str {
        // Bilingual (FR + EN) so French queries route correctly
        // without an English prefix.
        "Renvoie la date et l'heure courantes, éventuellement dans un fuseau horaire IANA \
         (Europe/Paris, America/New_York, Asia/Tokyo). Use for 'quelle heure est-il', \
         'what time is it in Tokyo', 'date du jour'. Pass `timezone` (optional IANA name, omit for UTC)."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "Optional IANA timezone name (e.g. Europe/Paris, America/New_York, Asia/Tokyo). Omit for UTC."
                }
            },
            "additionalProperties": false,
        })
    }

    async fn invoke(&self, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let now_utc: DateTime<Utc> = Utc::now();

        let payload = match req.timezone.as_deref() {
            None | Some("") => {
                json!({
                    "iso": now_utc.to_rfc3339(),
                    "unix": now_utc.timestamp(),
                    "timezone": "UTC",
                    "utc_offset": "+00:00",
                    "weekday": now_utc.format("%A").to_string(),
                })
            }
            Some(tz_str) => {
                // Resolve the IANA name. `chrono_tz::Tz::from_str` is
                // the supported entry point and returns an
                // `InvalidIANAName`-style error for unknown zones.
                let tz: Tz = tz_str.parse().map_err(|_| {
                    AgentError::InvalidArguments(format!(
                        "unknown IANA timezone `{tz_str}` (expected e.g. Europe/Paris, \
                         America/New_York, Asia/Tokyo)"
                    ))
                })?;
                let localised = now_utc.with_timezone(&tz);
                json!({
                    "iso": localised.to_rfc3339(),
                    "unix": now_utc.timestamp(),
                    "timezone": tz_str,
                    "utc_offset": localised.format("%:z").to_string(),
                    "weekday": localised.format("%A").to_string(),
                })
            }
        };

        Ok(serde_json::to_string(&json!({
            "ok": true,
            "data": payload,
            "source": "system",
            "fetched_at": now_utc.to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug, Default)]
struct ParsedArgs {
    timezone: Option<String>,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let timezone = obj
        .get("timezone")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(ParsedArgs { timezone })
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn name_and_schema_are_stable() {
        // The tool name and schema are part of the LLM-facing wire
        // contract — a rename here is a breaking change for every
        // chat history that referenced the old name.
        let agent = DateTimeAgent::new();
        assert_eq!(agent.name(), "get_datetime");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["timezone"].is_object());
    }

    #[tokio::test]
    async fn invoke_utc_when_timezone_omitted() {
        let agent = DateTimeAgent::new();
        let result = agent.invoke(json!({})).await.expect("invoke");
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["source"], "system");
        assert_eq!(parsed["data"]["timezone"], "UTC");
        assert_eq!(parsed["data"]["utc_offset"], "+00:00");
        assert!(parsed["data"]["unix"].as_i64().unwrap() > 0);
        assert!(parsed["data"]["iso"].as_str().unwrap().contains('T'));
    }

    #[tokio::test]
    async fn invoke_paris_returns_positive_offset() {
        let agent = DateTimeAgent::new();
        let result = agent
            .invoke(json!({"timezone": "Europe/Paris"}))
            .await
            .expect("invoke");
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["data"]["timezone"], "Europe/Paris");
        // Paris is UTC+1 (winter) or UTC+2 (summer). We accept either.
        let offset = parsed["data"]["utc_offset"].as_str().unwrap();
        assert!(
            offset == "+01:00" || offset == "+02:00",
            "expected Europe/Paris offset to be +01:00 or +02:00, got {offset}"
        );
    }

    #[tokio::test]
    async fn invoke_rejects_unknown_timezone() {
        let agent = DateTimeAgent::new();
        let err = agent
            .invoke(json!({"timezone": "Mars/Olympus_Mons"}))
            .await
            .expect_err("should reject");
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("unknown IANA timezone"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invoke_rejects_non_object_arguments() {
        let agent = DateTimeAgent::new();
        let err = agent
            .invoke(json!("not-an-object"))
            .await
            .expect_err("should reject");
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn invoke_treats_empty_timezone_as_utc() {
        // `{"timezone": ""}` should behave like `{}`. The arg parser
        // trims and filters empty strings, so the agent falls through
        // to the UTC branch.
        let agent = DateTimeAgent::new();
        let result = agent.invoke(json!({"timezone": "  "})).await.unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["data"]["timezone"], "UTC");
    }
}
