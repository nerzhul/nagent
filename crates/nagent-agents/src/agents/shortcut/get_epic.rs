//! `shortcut_get_epic` — `GET /epics/{id}`: one epic in full,
//! with the description capped at `[agents.shortcut].max_body_bytes`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{cap_text, confirm_reason, label_names, parse_id, run};
use crate::agents::{Agent, AgentError, UserContext};

pub struct GetEpicAgent {
    client: ShortcutClient,
}

impl GetEpicAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for GetEpicAgent {
    fn name(&self) -> &str {
        "shortcut_get_epic"
    }

    fn description(&self) -> &str {
        "Fetch one Shortcut epic in full (`GET /epics/{id}`): name, state, labels, deadline, \
         stats, and the capped description. Args: `epic_id` (positive integer, as returned by \
         `shortcut_list_epics` / story `epic_id` fields). Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "epic_id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Epic id (positive integer).",
                },
            },
            "required": ["epic_id"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "shortcut",
            "epic",
            "detail",
            "get",
            "description",
            "initiative",
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
        let epic_id = parse_id(&args, "epic_id")?;
        let path = format!("/epics/{epic_id}");
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            &path,
            Vec::new(),
            move |raw, cfg| {
                json!({
                    "ok": true,
                    "data": {
                        "id": raw.get("id"),
                        "name": raw.get("name"),
                        "state": raw.get("state"),
                        "labels": label_names(raw),
                        "deadline": raw.get("deadline"),
                        "archived": raw.get("archived"),
                        "stats": raw.get("stats"),
                        "description": cap_text(
                            &raw.get("description").cloned().unwrap_or(Value::Null),
                            cfg.max_body_bytes,
                        ),
                        "app_url": raw.get("app_url"),
                        "created_at": raw.get("created_at"),
                        "updated_at": raw.get("updated_at"),
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
    fn schema_requires_epic_id() {
        let agent = GetEpicAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        assert_eq!(
            agent.parameters_schema()["required"],
            serde_json::json!(["epic_id"])
        );
    }
}
