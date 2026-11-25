//! `shortcut_get_story` — `GET /stories/{id}` plus
//! `GET /stories/{id}/comments`: one story in full, with the
//! description and each comment text capped at
//! `[agents.shortcut].max_body_bytes`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{cap_text, confirm_reason, label_names, parse_id, project_story};
use crate::agents::{Agent, AgentError, UserContext};

pub struct GetStoryAgent {
    client: ShortcutClient,
}

impl GetStoryAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for GetStoryAgent {
    fn name(&self) -> &str {
        "shortcut_get_story"
    }

    fn description(&self) -> &str {
        "Fetch one Shortcut story in full (`GET /stories/{id}` plus `GET /stories/{id}/comments`): \
         all list fields plus the capped description and the capped comments. Args: `story_id` \
         (positive integer, as returned by `shortcut_list_backlog` / `shortcut_search_stories`). \
         Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "story_id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Story id (positive integer).",
                },
            },
            "required": ["story_id"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "shortcut",
            "story",
            "detail",
            "get",
            "description",
            "comments",
            "ticket",
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
        let story_id = parse_id(&args, "story_id")?;
        let tool = self.name();
        // Tool-level cache: one entry covers the story + comments pair.
        if let Some(cached) = self.client.cache_get(ctx.user_id(), tool, &args) {
            tracing::debug!(agent = tool, user_id = %ctx.user_id(), "shortcut cache hit");
            return Ok(cached);
        }
        let story = self
            .client
            .get(ctx, &format!("/stories/{story_id}"), &[])
            .await?;
        let comments = self
            .client
            .get(ctx, &format!("/stories/{story_id}/comments"), &[])
            .await?;
        let cfg = &self.client.cfg;
        let out = json!({
            "ok": true,
            "data": {
                "story": project_story(&story),
                "description": cap_text(
                    &story.get("description").cloned().unwrap_or(Value::Null),
                    cfg.max_body_bytes,
                ),
                "labels": label_names(&story),
                "comments": comments
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|c| {
                                json!({
                                    "id": c.get("id"),
                                    "author_id": c.get("author_id"),
                                    "text": cap_text(
                                        &c.get("text").cloned().unwrap_or(Value::Null),
                                        cfg.max_body_bytes,
                                    ),
                                    "created_at": c.get("created_at"),
                                    "updated_at": c.get("updated_at"),
                                })
                            })
                            .collect::<Vec<Value>>()
                    })
                    .unwrap_or_default(),
            }
        });
        let encoded = serde_json::to_string(&out)
            .map_err(|e| AgentError::AgentFailed(format!("shortcut encode: {e}")))?;
        self.client
            .cache_put(ctx.user_id(), tool, &args, encoded.clone());
        Ok(encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_requires_story_id() {
        let agent = GetStoryAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        assert_eq!(
            agent.parameters_schema()["required"],
            serde_json::json!(["story_id"])
        );
    }
}
