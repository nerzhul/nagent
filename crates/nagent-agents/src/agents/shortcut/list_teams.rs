//! `shortcut_list_teams` — `GET /groups`:
//! the workspace's groups, projected for the chat. Each group carries
//! its UUID `id` (sent as `group_id` when creating a story), its
//! `name`, and its `mention_name` (the handle used in `team:<…>`
//! search operators). Pairs with `shortcut_create_story`'s `team`
//! argument to resolve a group name to its UUID, and with the
//! integrations UI's "default group" picker.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{cached_get_groups, confirm_reason, project_team};
use crate::agents::{Agent, AgentError, UserContext};

pub struct ListTeamsAgent {
    client: ShortcutClient,
}

impl ListTeamsAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for ListTeamsAgent {
    fn name(&self) -> &str {
        "shortcut_list_teams"
    }

    fn description(&self) -> &str {
        "List the Shortcut groups of the workspace (`GET /groups`) with their UUID `id`, \
         `name`, `mention_name`, and `workflow_ids`. Use it to resolve a group name to the \
         UUID that `shortcut_create_story`'s `team` / `group_id` expects, or to pick the \
         `default_team` saved in the integrations UI. No args. Read-only."
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
            "team",
            "teams",
            "group",
            "groups",
            "workspace",
            "default_team",
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

    async fn invoke(&self, ctx: &UserContext, _args: Value) -> Result<String, AgentError> {
        let raw = cached_get_groups(&self.client, ctx, self.name()).await?;
        let teams: Vec<Value> = raw
            .as_array()
            .map(|arr| arr.iter().map(project_team).collect())
            .unwrap_or_default();
        let out = json!({
            "ok": true,
            "data": {
                "count": teams.len(),
                "teams": teams,
            }
        });
        serde_json::to_string(&out)
            .map_err(|e| AgentError::AgentFailed(format!("shortcut encode: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_takes_no_args() {
        let agent = ListTeamsAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let schema = agent.parameters_schema();
        assert_eq!(schema["properties"], json!({}));
    }
}
