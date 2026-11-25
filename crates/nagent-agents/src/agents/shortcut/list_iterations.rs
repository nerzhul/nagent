//! `shortcut_list_iterations` — `GET /iterations`: the current
//! and upcoming iterations (done iterations are filtered out
//! client-side), sliced to `limit`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{confirm_reason, label_names, parse_limit, run};
use crate::agents::{Agent, AgentError, UserContext};

pub struct ListIterationsAgent {
    client: ShortcutClient,
}

impl ListIterationsAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

fn project_iteration(i: &Value) -> Value {
    json!({
        "id": i.get("id"),
        "name": i.get("name"),
        "status": i.get("status"),
        "start_date": i.get("start_date"),
        "end_date": i.get("end_date"),
        "labels": label_names(i),
        "stats": i.get("stats"),
        "app_url": i.get("app_url"),
        "created_at": i.get("created_at"),
        "updated_at": i.get("updated_at"),
    })
}

#[async_trait]
impl Agent for ListIterationsAgent {
    fn name(&self) -> &str {
        "shortcut_list_iterations"
    }

    fn description(&self) -> &str {
        "List the Shortcut iterations relevant for planning (`GET /iterations`): current and \
         upcoming iterations with id, name, status, start/end dates and stats — past \
         (`done`) iterations are excluded. Use to know which iteration is running now and \
         what is scheduled next when proposing a plan for the next iteration. Args: `limit?` \
         (default 20, server cap [agents.shortcut].max_items = 200). Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "default": 20,
                    "description": "Maximum number of iterations to return. Server cap is [agents.shortcut].max_items.",
                },
            },
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "shortcut",
            "iterations",
            "sprints",
            "list",
            "current",
            "upcoming",
            "schedule",
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
        let limit = parse_limit(&args, &self.client.cfg, 20);
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            "/iterations",
            Vec::new(),
            move |raw, _cfg| {
                let all: Vec<Value> = raw
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter(|i| i.get("status").and_then(|v| v.as_str()) != Some("done"))
                            .map(project_iteration)
                            .collect()
                    })
                    .unwrap_or_default();
                let iterations: Vec<Value> = all.into_iter().take(limit).collect();
                json!({
                    "ok": true,
                    "data": {
                        "count": iterations.len(),
                        "iterations": iterations,
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
    fn project_iteration_picks_llm_fields() {
        let raw = json!({
            "id": 55,
            "name": "Sprint 42",
            "status": "started",
            "start_date": "2026-11-17",
            "end_date": "2026-11-30",
            "stats": {"num_stories_total": 12},
            "internal_field": "dropped"
        });
        let p = project_iteration(&raw);
        assert_eq!(p["id"], 55);
        assert_eq!(p["status"], "started");
        assert!(p.get("internal_field").is_none());
    }
}
