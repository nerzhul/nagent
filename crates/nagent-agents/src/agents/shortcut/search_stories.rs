//! `shortcut_search_stories` — `GET /search/stories` with
//! Shortcut search syntax. The query is passed through verbatim
//! (percent-encoded on the wire); operators include `is:story`,
//! `is:unstarted` / `is:started` / `is:done`, `state:"Ready for
//! dev"`, `owner:jane`, `team:Engineering`, `epic:growth`,
//! `label:release`, `project:Mobile`, `estimate:3`, `type:bug`,
//! and `!` / `-` negation (e.g. `!is:archived`).

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{confirm_reason, parse_limit, parse_required_str, project_story, run};
use crate::agents::{Agent, AgentError, UserContext};

pub struct SearchStoriesAgent {
    client: ShortcutClient,
}

impl SearchStoriesAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for SearchStoriesAgent {
    fn name(&self) -> &str {
        "shortcut_search_stories"
    }

    fn description(&self) -> &str {
        "Search Shortcut stories with Shortcut search syntax (`GET /search/stories?query=...`). \
         Operators: `is:story`, `is:unstarted` / `is:started` / `is:done`, \
         `state:\"Ready for dev\"`, `owner:jane` (mention name without @), `team:Engineering`, \
         `epic:growth`, `label:release`, `project:Mobile`, `estimate:3`, `type:bug|feature|chore`, \
         `has:epic`, `created:2026-01-01..*`, and `!` / `-` negation (e.g. `!is:archived`). \
         Multiple operators combine with AND. Args: `query` (required), `limit?` (default 30, \
         sent as `page_size`, server cap [agents.shortcut].max_items = 200). Returns `total` \
         plus the matching stories. Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Shortcut search query, e.g. `is:story is:started owner:jane` or `is:story project:Mobile !is:archived`.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 200,
                    "default": 30,
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "shortcut",
            "search",
            "stories",
            "find",
            "query",
            "is:story",
            "is:backlog",
            "owner:",
            "state:",
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
        let query = parse_required_str(&args, "query")?;
        let limit = parse_limit(&args, &self.client.cfg, 30);
        let q = vec![
            ("query".to_string(), query),
            ("page_size".to_string(), limit.to_string()),
        ];
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            "/search/stories",
            q,
            |raw, _cfg| {
                let stories: Vec<Value> = raw
                    .get("data")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().map(project_story).collect())
                    .unwrap_or_default();
                json!({
                    "ok": true,
                    "data": {
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
    fn schema_requires_query() {
        let agent = SearchStoriesAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        assert_eq!(
            agent.parameters_schema()["required"],
            serde_json::json!(["query"])
        );
    }
}
