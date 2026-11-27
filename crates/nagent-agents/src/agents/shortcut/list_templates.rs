//! `shortcut_list_templates` — list the user's saved Shortcut story
//! shortcuts (templates).
//!
//! Unlike every other `shortcut_*` tool this one is a **local**
//! read: it parses the per-user vault field `story_templates`
//! (a JSON array) and never calls the Shortcut API. The content is
//! the user's own saved config, so — like `get_datetime` — it needs
//! no confirmation and its output is not wrapped in an untrusted
//! fence.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::load_templates;
use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};

pub struct ListTemplatesAgent;

impl ListTemplatesAgent {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ListTemplatesAgent {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Agent for ListTemplatesAgent {
    fn name(&self) -> &str {
        "shortcut_list_templates"
    }

    fn description(&self) -> &str {
        "List the user's saved Shortcut story shortcuts (templates), read locally from the \
         per-user `story_templates` vault field — no Shortcut API call. Each template is \
         `{name, story_type?, labels?, description_template?}`. Pass a template's `name` to \
         `shortcut_create_story`'s `template` argument to create a story with that \
         template's defaults. No args. Local read."
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
            "template",
            "templates",
            "story shortcut",
            "raccourci",
        ]
    }

    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        // Local read of the user's own vault field: no third-party
        // call, no confirmation (same posture as `get_datetime`).
        ConfirmationDecision::Allow
    }

    fn untrusted_output(&self) -> bool {
        // The templates come from the caller's own saved config,
        // not from a remote document, so the output is trusted.
        false
    }

    async fn invoke(&self, ctx: &UserContext, _args: Value) -> Result<String, AgentError> {
        let templates = load_templates(ctx).await?;
        let names: Vec<&str> = templates
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        serde_json::to_string(&json!({
            "ok": true,
            "data": {
                "count": templates.len(),
                "templates": templates,
                "names": names,
            }
        }))
        .map_err(|e| AgentError::AgentFailed(format!("shortcut encode: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{Agent, SecretSource};
    use crate::ServiceRegistry;
    use std::sync::Arc;

    const TEMPLATES_JSON: &str = concat!(
        r###"[{"name":"bug","story_type":"bug","labels":["bug"],"###,
        r###""description_template":"## Steps\n{details}"}]"###
    );

    struct TemplatesSecrets;

    #[async_trait::async_trait]
    impl SecretSource for TemplatesSecrets {
        async fn fetch(
            &self,
            _user_id: uuid::Uuid,
            service: &str,
            field: &str,
        ) -> Result<Option<secrecy::SecretString>, AgentError> {
            if service == "shortcut" && field == "story_templates" {
                Ok(Some(secrecy::SecretString::from(TEMPLATES_JSON)))
            } else {
                Ok(None)
            }
        }
    }

    struct EmptySecrets;

    #[async_trait::async_trait]
    impl SecretSource for EmptySecrets {
        async fn fetch(
            &self,
            _user_id: uuid::Uuid,
            _service: &str,
            _field: &str,
        ) -> Result<Option<secrecy::SecretString>, AgentError> {
            Ok(None)
        }
    }

    fn ctx_with<S: SecretSource + 'static>(secrets: S) -> UserContext {
        UserContext::new(
            uuid::Uuid::new_v4(),
            Arc::new(ServiceRegistry::empty()),
            Arc::new(secrets),
        )
    }

    #[test]
    fn local_read_needs_no_confirmation_and_is_trusted() {
        let agent = ListTemplatesAgent::new();
        let d = agent.requires_confirmation(&ctx_with(EmptySecrets), &json!({}));
        assert!(d.is_allowed());
        assert!(!agent.untrusted_output());
    }

    #[tokio::test]
    async fn invoke_returns_parsed_templates() {
        let agent = ListTemplatesAgent::new();
        let out = agent
            .invoke(&ctx_with(TemplatesSecrets), json!({}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["count"], 1);
        assert_eq!(v["data"]["templates"][0]["name"], "bug");
        assert_eq!(v["data"]["names"][0], "bug");
    }

    #[tokio::test]
    async fn missing_field_yields_empty_list() {
        let agent = ListTemplatesAgent::new();
        let out = agent
            .invoke(&ctx_with(EmptySecrets), json!({}))
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["data"]["count"], 0);
    }
}
