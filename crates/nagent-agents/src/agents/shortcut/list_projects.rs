//! `shortcut_list_projects` — `GET /projects`: the workspace's
//! projects, sliced to `limit` client-side (the v3 list endpoint
//! is not paginated).

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{confirm_reason, parse_limit, run};
use crate::agents::{Agent, AgentError, UserContext};

pub struct ListProjectsAgent {
    client: ShortcutClient,
}

impl ListProjectsAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

fn project_project(p: &Value) -> Value {
    json!({
        "id": p.get("id"),
        "name": p.get("name"),
        "description": p.get("description"),
        "state": p.get("state"),
        "archived": p.get("archived"),
        "app_url": p.get("app_url"),
        "created_at": p.get("created_at"),
        "updated_at": p.get("updated_at"),
    })
}

#[async_trait]
impl Agent for ListProjectsAgent {
    fn name(&self) -> &str {
        "shortcut_list_projects"
    }

    fn description(&self) -> &str {
        "List the Shortcut projects in the workspace (`GET /projects`) with id, name, \
         description, state and archived flag. Use to translate a `project_id` returned by \
         the story tools into a project name, or to scope a plan to one project. Args: \
         `limit?` (default 50, server cap [agents.shortcut].max_items = 200). Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "default": 50,
                    "description": "Maximum number of projects to return. Server cap is [agents.shortcut].max_items.",
                },
            },
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &["shortcut", "projects", "list", "workspace", "product"]
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
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            "/projects",
            Vec::new(),
            move |raw, _cfg| {
                let all: Vec<Value> = raw
                    .as_array()
                    .map(|arr| arr.iter().map(project_project).collect())
                    .unwrap_or_default();
                let projects: Vec<Value> = all.into_iter().take(limit).collect();
                json!({
                    "ok": true,
                    "data": {
                        "count": projects.len(),
                        "projects": projects,
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
    fn schema_locks_default_limit() {
        let agent = ListProjectsAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let schema = agent.parameters_schema();
        assert_eq!(schema["properties"]["limit"]["default"], 50);
        assert_eq!(schema["properties"]["limit"]["maximum"], 200);
    }
}
