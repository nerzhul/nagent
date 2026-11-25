//! `shortcut_list_epics` — `GET /epics`: the workspace's epics,
//! optionally filtered client-side by `state`
//! (unstarted/started/done) and sliced to `limit` (the v3 list
//! endpoint is not paginated and takes no state query param, so
//! the filter is applied on the returned `state` field).

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{confirm_reason, label_names, parse_enum, parse_limit, run};
use crate::agents::{Agent, AgentError, UserContext};

const STATES: &[&str] = &["unstarted", "started", "done"];

pub struct ListEpicsAgent {
    client: ShortcutClient,
}

impl ListEpicsAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

fn project_epic(e: &Value) -> Value {
    json!({
        "id": e.get("id"),
        "name": e.get("name"),
        "state": e.get("state"),
        "description": e.get("description"),
        "deadline": e.get("deadline"),
        "archived": e.get("archived"),
        "labels": label_names(e),
        "stats": e.get("stats"),
        "app_url": e.get("app_url"),
        "created_at": e.get("created_at"),
        "updated_at": e.get("updated_at"),
    })
}

#[async_trait]
impl Agent for ListEpicsAgent {
    fn name(&self) -> &str {
        "shortcut_list_epics"
    }

    fn description(&self) -> &str {
        "List the Shortcut epics in the workspace (`GET /epics`) with id, name, state \
         (unstarted/started/done), labels, deadline and stats. Args: `state?` \
         (unstarted/started/done, default all — filtered client-side), `limit?` \
         (default 50, server cap [agents.shortcut].max_items = 200). Use with \
         `shortcut_get_epic` to drill into one epic's capped description. Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "state": {
                    "type": "string",
                    "enum": ["unstarted", "started", "done"],
                    "description": "Filter by epic state. Omit for all epics.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "default": 50,
                    "description": "Maximum number of epics to return. Server cap is [agents.shortcut].max_items.",
                },
            },
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "shortcut",
            "epics",
            "list",
            "initiatives",
            "themes",
            "state",
        ]
    }

    fn requires_confirmation(
        &self,
        _ctx: &UserContext,
        _args: &Value,
    ) -> crate::agents::ConfirmationDecision {
        confirm_reason(self.name())
    }

    fn untrusted_output(&self) -> bool {
        true
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let limit = parse_limit(&args, &self.client.cfg, 50);
        let state = parse_enum(&args, "state", STATES, "")?;
        let state_filter = if state.is_empty() { None } else { Some(state) };
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            "/epics",
            Vec::new(),
            move |raw, _cfg| {
                let all: Vec<Value> = raw
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter(|e| {
                                state_filter
                                    .as_ref()
                                    .map(|s| {
                                        e.get("state").and_then(|v| v.as_str()) == Some(s.as_str())
                                    })
                                    .unwrap_or(true)
                            })
                            .map(project_epic)
                            .collect()
                    })
                    .unwrap_or_default();
                let epics: Vec<Value> = all.into_iter().take(limit).collect();
                json!({
                    "ok": true,
                    "data": {
                        "count": epics.len(),
                        "epics": epics,
                    }
                })
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_epic_picks_llm_fields() {
        let raw = json!({
            "id": 7,
            "name": "Mobile app",
            "state": "started",
            "labels": [{"name": "mobile"}],
            "stats": {"num_stories_total": 42},
            "internal_field": "dropped"
        });
        let p = project_epic(&raw);
        assert_eq!(p["id"], 7);
        assert_eq!(p["state"], "started");
        assert_eq!(p["labels"], json!(["mobile"]));
        assert!(p.get("internal_field").is_none());
    }

    #[test]
    fn schema_locks_state_enum() {
        let agent = ListEpicsAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        assert_eq!(
            agent.parameters_schema()["properties"]["state"]["enum"],
            json!(["unstarted", "started", "done"])
        );
    }
}
