//! `shortcut_me` — the member the API token belongs to (id, mention
//! name, display name, email). No arguments. Calls `GET /members`
//! (the v3 API has no `/members/me` route — that path falls through
//! to `/members/{member-public-id}` and 400s on the UUID spec) and
//! picks the member whose mention name matches the stored
//! `username` credential.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{confirm_reason, find_member, mention_name_of, name_of_member, run};
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
        "Return the Shortcut member the connected API token belongs to (`GET /members`, matched \
         by the stored mention name): id, mention name, display name, email. Use to answer \
         \"which Shortcut account am I connected as?\" or to ground `owner:` / `requester:` \
         search queries (they take the mention name without @). Read-only; requires the \
         Shortcut integration to be configured in /settings/integrations."
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
        use secrecy::ExposeSecret;
        // The stored `username` is the mention name the setup probe
        // resolved; it identifies the token's member in the list.
        let username = ctx
            .secret("shortcut", "username")
            .await?
            .map(|s| s.expose_secret().trim().to_string());
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            "/members",
            Vec::new(),
            |raw, _cfg| match find_member(raw, username.as_deref()) {
                Some(m) => json!({
                    "ok": true,
                    "data": {
                        "id": m.get("id"),
                        "mention_name": mention_name_of(m),
                        "name": name_of_member(m),
                        "email": m.get("profile").and_then(|p| p.get("email_address")),
                        "disabled": m.get("disabled"),
                    }
                }),
                None => json!({
                    "ok": false,
                    "error": "could not match a workspace member to the stored mention name; \
                              re-run the Shortcut probe in /settings/integrations"
                }),
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
