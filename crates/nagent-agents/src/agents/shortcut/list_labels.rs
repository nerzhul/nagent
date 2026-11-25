//! `shortcut_list_labels` — `GET /labels`: the workspace's
//! labels, sliced to `limit` client-side.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{confirm_reason, parse_limit, run};
use crate::agents::{Agent, AgentError, UserContext};

pub struct ListLabelsAgent {
    client: ShortcutClient,
}

impl ListLabelsAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

fn project_label(l: &Value) -> Value {
    json!({
        "id": l.get("id"),
        "name": l.get("name"),
        "color": l.get("color"),
        "description": l.get("description"),
        "archived": l.get("archived"),
        "app_url": l.get("app_url"),
    })
}

#[async_trait]
impl Agent for ListLabelsAgent {
    fn name(&self) -> &str {
        "shortcut_list_labels"
    }

    fn description(&self) -> &str {
        "List the Shortcut labels in the workspace (`GET /labels`) with id, name and color. \
         Use to translate the `labels` arrays returned by the story / epic tools into \
         human-readable names, and to ground `label:` search queries. Args: `limit?` \
         (default 100, server cap [agents.shortcut].max_items = 200). Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "default": 100,
                    "description": "Maximum number of labels to return. Server cap is [agents.shortcut].max_items.",
                },
            },
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &["shortcut", "labels", "tags", "list", "label:"]
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
        let limit = parse_limit(&args, &self.client.cfg, 100);
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            "/labels",
            Vec::new(),
            move |raw, _cfg| {
                let all: Vec<Value> = raw
                    .as_array()
                    .map(|arr| arr.iter().map(project_label).collect())
                    .unwrap_or_default();
                let labels: Vec<Value> = all.into_iter().take(limit).collect();
                json!({
                    "ok": true,
                    "data": {
                        "count": labels.len(),
                        "labels": labels,
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
    fn project_label_picks_llm_fields() {
        let raw = json!({"id": 3, "name": "backend", "color": "#ff0000", "internal": 1});
        let p = project_label(&raw);
        assert_eq!(p["name"], "backend");
        assert!(p.get("internal").is_none());
    }
}
