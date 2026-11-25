//! `shortcut_list_workflows` — `GET /workflows`: every workflow
//! with its states projected as a state id → name/category map,
//! which is what the LLM needs to interpret `workflow_state_id`
//! values returned by the story tools.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{confirm_reason, run};
use crate::agents::{Agent, UserContext};

pub struct ListWorkflowsAgent {
    client: ShortcutClient,
}

impl ListWorkflowsAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

/// Project one workflow into the LLM-facing shape: its states as
/// `[{id, name, category, position}]`.
fn project_workflow(w: &Value) -> Value {
    let states: Vec<Value> = w
        .get("states")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .map(|s| {
                    json!({
                        "id": s.get("id"),
                        "name": s.get("name"),
                        "category": s.get("category"),
                        "position": s.get("position"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": w.get("id"),
        "name": w.get("name"),
        "states": states,
    })
}

#[async_trait]
impl Agent for ListWorkflowsAgent {
    fn name(&self) -> &str {
        "shortcut_list_workflows"
    }

    fn description(&self) -> &str {
        "List every Shortcut workflow with its states (`GET /workflows`), projected as \
         state id → {name, category} per workflow. Workflow state categories are \
         `unstarted` / `started` / `done`; the backlog lives in the `unstarted` states. \
         Use this to translate the `workflow_state_id` field of stories returned by \
         `shortcut_list_backlog` / `shortcut_search_stories` / `shortcut_get_story` into \
         human-readable column names. No arguments. Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "shortcut",
            "workflow",
            "state",
            "states",
            "columns",
            "board",
            "categories",
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

    async fn invoke(
        &self,
        ctx: &UserContext,
        args: Value,
    ) -> Result<String, crate::agents::AgentError> {
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            "/workflows",
            Vec::new(),
            |raw, _cfg| {
                let workflows: Vec<Value> = raw
                    .as_array()
                    .map(|arr| arr.iter().map(project_workflow).collect())
                    .unwrap_or_default();
                json!({
                    "ok": true,
                    "data": {
                        "count": workflows.len(),
                        "workflows": workflows,
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

    #[test]
    fn project_workflow_lists_states_with_category() {
        let raw = json!({
            "id": 1,
            "name": "Engineering",
            "states": [
                {"id": 500000001, "name": "Unscheduled", "category": "unstarted", "position": 1},
                {"id": 500000002, "name": "Ready for Dev", "category": "unstarted", "position": 2},
                {"id": 500000003, "name": "In Development", "category": "started", "position": 3}
            ]
        });
        let p = project_workflow(&raw);
        assert_eq!(p["name"], "Engineering");
        assert_eq!(p["states"].as_array().unwrap().len(), 3);
        assert_eq!(p["states"][0]["category"], "unstarted");
        assert_eq!(p["states"][0]["id"], 500000001);
    }
}
