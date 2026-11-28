//! `github_list_accounts` — vault metadata for every configured
//! GitHub account (plan 1795793328177 §2.4). No network call: the
//! rows come from `github.personal_access_token` (the `"default"`
//! account), the `github.accounts` JSON blob, and the
//! `github.default_account` pointer. **Never returns tokens**
//! (AGENTS.md §6) — this is how the LLM discovers which accounts
//! exist before calling another tool with `account: "all"` or a
//! name.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::accounts;
use super::client::GithubClient;
use super::confirm_reason;
use crate::agents::{Agent, AgentError, UserContext};

pub struct ListAccountsAgent {
    client: GithubClient,
}

impl ListAccountsAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for ListAccountsAgent {
    fn name(&self) -> &str {
        "github_list_accounts"
    }

    fn description(&self) -> &str {
        "List the GitHub accounts configured for this user: one entry per account with \
         `name`, `username` (the GitHub login), `scopes` (the token's OAuth scopes, \
         when known), and `is_default`. Tokens are never returned. Use this first to \
         discover which `account` names the other `github_*` tools accept, then pass \
         `account: \"<name>\"` or `account: \"all\"` to them. Read-only; no network \
         call — the rows come from the user's saved vault."
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
            "github",
            "accounts",
            "tokens",
            "connected",
            "list",
            "who",
            "default",
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
        let metas = accounts::list_accounts(ctx, &self.client.cfg).await?;
        let rows: Vec<Value> = metas.iter().map(|m| m.to_json()).collect();
        // Envelope is tiny and vault-sourced; cache it under the
        // default account so repeated discovery calls stay free.
        let out = json!({
            "ok": true,
            "data": {
                "count": rows.len(),
                "accounts": rows,
            }
        });
        let encoded = serde_json::to_string(&out)
            .map_err(|e| AgentError::AgentFailed(format!("github encode: {e}")))?;
        Ok(encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{ConfirmationDecision, SecretSource};
    use crate::ServiceRegistry;
    use secrecy::SecretString;
    use std::sync::Arc;

    const PAT: &str = "ghp_list_accounts_pat_0000000000";
    const TOK_A: &str = "ghp_list_accounts_a_aaaaaaaaaaaa";

    struct StaticSecrets;

    #[async_trait::async_trait]
    impl SecretSource for StaticSecrets {
        async fn fetch(
            &self,
            _user_id: uuid::Uuid,
            service: &str,
            field: &str,
        ) -> Result<Option<SecretString>, AgentError> {
            if service != "github" {
                return Ok(None);
            }
            let v = match field {
                "personal_access_token" => Some(PAT.to_string()),
                "username" => Some("octo".to_string()),
                "accounts" => Some(format!(
                    "[{{\"name\":\"client-a\",\"token\":\"{TOK_A}\",\"username\":\"octo-a\",\"scopes\":\"repo\"}}]"
                )),
                "default_account" => Some("client-a".to_string()),
                _ => None,
            };
            Ok(v.map(SecretString::from))
        }
    }

    fn ctx() -> UserContext {
        UserContext::new(
            uuid::Uuid::new_v4(),
            Arc::new(ServiceRegistry::empty()),
            Arc::new(StaticSecrets),
        )
    }

    #[test]
    fn requires_confirmation_is_strict() {
        let agent = ListAccountsAgent::new(GithubClient::new(
            crate::GithubAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let ctx = ctx();
        let d = agent.requires_confirmation(&ctx, &Value::Null);
        assert!(matches!(d, ConfirmationDecision::NeedsConfirmation { .. }));
    }

    #[tokio::test]
    async fn invoke_lists_accounts_without_tokens() {
        let agent = ListAccountsAgent::new(GithubClient::new(
            crate::GithubAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let out = agent.invoke(&ctx(), json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["count"], 2);
        assert_eq!(parsed["data"]["accounts"][0]["name"], "default");
        assert_eq!(parsed["data"]["accounts"][0]["username"], "octo");
        assert_eq!(parsed["data"]["accounts"][1]["name"], "client-a");
        assert_eq!(parsed["data"]["accounts"][1]["is_default"], true);
        assert_eq!(parsed["data"]["accounts"][1]["scopes"], "repo");
        // AGENTS.md §6: no token anywhere in the response.
        assert!(
            !out.contains(PAT) && !out.contains(TOK_A),
            "github_list_accounts must never return a token; got {out}"
        );
    }
}
