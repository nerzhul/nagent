//! `shortcut_list_backlog` — the core "read my backlog" tool:
//! `GET /search/stories` with the operator-configured
//! `[agents.shortcut].backlog_query` (default `is:story is:backlog
//! !is:archived` — stories in an unstarted workflow state, not in
//! an iteration, not archived).

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{confirm_reason, parse_limit, project_story, run};
use crate::agents::{Agent, AgentError, UserContext};

pub struct ListBacklogAgent {
    client: ShortcutClient,
}

impl ListBacklogAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for ListBacklogAgent {
    fn name(&self) -> &str {
        "shortcut_list_backlog"
    }

    fn description(&self) -> &str {
        "List the user's Shortcut backlog (`GET /search/stories` with the operator-configured \
         `[agents.shortcut].backlog_query`, default `is:story is:backlog !is:archived` — stories \
         in an unstarted workflow state, not scheduled in an iteration, not archived). This is \
         the main entry point for \"what's in my backlog?\" / \"propose a plan for the next \
         iteration\" questions. Pair with `shortcut_list_workflows` to resolve \
         `workflow_state_id` values, and with `shortcut_get_story` / `shortcut_get_epic` to \
         drill into an item. Args: `limit?` (default 50, server cap 200). Returns the \
         matching stories plus the query that was run. Read-only."
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
                    "description": "Maximum number of stories to return. Server cap is [agents.shortcut].max_items (200).",
                },
            },
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "shortcut",
            "backlog",
            "stories",
            "unscheduled",
            "todo",
            "plan",
            "is:backlog",
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
        let query = self.client.cfg.backlog_query.clone();
        let q = vec![
            ("query".to_string(), query.clone()),
            ("page_size".to_string(), limit.to_string()),
        ];
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            "/search/stories",
            q,
            move |raw, _cfg| {
                let stories: Vec<Value> = raw
                    .get("data")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().map(project_story).collect())
                    .unwrap_or_default();
                json!({
                    "ok": true,
                    "data": {
                        "query": query,
                        "total": raw.get("total"),
                        "count": stories.len(),
                        "stories": stories,
                        "next": raw.get("next"),
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
    fn schema_defaults_limit_to_50() {
        let agent = ListBacklogAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let schema = agent.parameters_schema();
        assert_eq!(schema["properties"]["limit"]["default"], 50);
        assert_eq!(schema["properties"]["limit"]["maximum"], 200);
        assert!(schema.get("required").is_none());
    }
}
