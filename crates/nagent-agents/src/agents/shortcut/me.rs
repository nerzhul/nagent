//! `shortcut_me` — `GET /members/me`: the member the API token
//! belongs to (id, mention name, display name, email). No
//! arguments.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{confirm_reason, mention_name_of, name_of_member, run};
use crate::agents::{Agent, UserContext};

pub struct MeAgent {
    client: ShortcutClient,
}

impl MeAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for MeAgent {
    fn name(&self) -> &str {
        "shortcut_me"
    }

    fn description(&self) -> &str {
        "Return the Shortcut member the connected API token belongs to (`GET /members/me`): \
         id, mention name, display name, email. Use to answer \"which Shortcut account am I \
         connected as?\" or to ground `owner:` / `requester:` search queries (they take the \
         mention name without @). Read-only; requires the Shortcut integration to be \
         configured in /settings/integrations."
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
            "shortcut", "account", "member", "whoami", "me", "mention", "profile",
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
            "/members/me",
            Vec::new(),
            |raw, _cfg| {
                json!({
                    "ok": true,
                    "data": {
                        "id": raw.get("id"),
                        "mention_name": mention_name_of(raw),
                        "name": name_of_member(raw),
                        "email": raw.get("profile").and_then(|p| p.get("email_address")),
                        "disabled": raw.get("disabled"),
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
    use crate::agents::{ConfirmationDecision, UserContext};
    use crate::ServiceRegistry;
    use std::sync::Arc;

    #[test]
    fn requires_confirmation_is_strict_even_in_same_turn() {
        let agent = MeAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let mut ctx =
            UserContext::for_tests(uuid::Uuid::new_v4(), Arc::new(ServiceRegistry::empty()));
        let d1 = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(matches!(d1, ConfirmationDecision::NeedsConfirmation { .. }));
        ctx.record_invocation(agent.name());
        let d2 = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(matches!(d2, ConfirmationDecision::NeedsConfirmation { .. }));
    }

    #[test]
    fn schema_has_no_properties() {
        let agent = MeAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"], json!({}));
    }
}
