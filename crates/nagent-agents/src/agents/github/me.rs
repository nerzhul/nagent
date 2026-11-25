//! `github_me` — `GET /user`: the authenticated user's GitHub
//! profile (login, name, plan, counts). No arguments.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{confirm_reason, run};
use crate::agents::{Agent, UserContext};

pub struct MeAgent {
    client: GithubClient,
}

impl MeAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for MeAgent {
    fn name(&self) -> &str {
        "github_me"
    }

    fn description(&self) -> &str {
        "Return the authenticated GitHub user's profile (`GET /user`): login, name, bio, \
         avatar URL, public repo count, followers/following, account creation date, and \
         plan. Use to answer \"which GitHub account am I connected as?\" or to ground \
         `author:@me` search queries. Read-only; requires the GitHub integration to be \
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
            "github", "account", "profile", "whoami", "me", "user", "login",
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
            "/user",
            Vec::new(),
            |raw, _cfg| {
                json!({
                    "ok": true,
                    "data": {
                        "login": raw.get("login"),
                        "name": raw.get("name"),
                        "bio": raw.get("bio"),
                        "avatar_url": raw.get("avatar_url"),
                        "html_url": raw.get("html_url"),
                        "type": raw.get("type"),
                        "public_repos": raw.get("public_repos"),
                        "followers": raw.get("followers"),
                        "following": raw.get("following"),
                        "created_at": raw.get("created_at"),
                        "plan": raw.get("plan"),
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
        let agent = MeAgent::new(GithubClient::new(
            crate::GithubAgentConfig::default(),
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
        let agent = MeAgent::new(GithubClient::new(
            crate::GithubAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"], json!({}));
    }
}
