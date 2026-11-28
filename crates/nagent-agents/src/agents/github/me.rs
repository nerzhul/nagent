//! `github_me` — `GET /user`: the authenticated user's GitHub
//! profile (login, name, plan, counts). With `account: "all"`,
//! fans out across every configured account (plan 1795793328177).

use async_trait::async_trait;
use serde_json::{json, Value};

use super::accounts;
use super::client::GithubClient;
use super::{account_prop, confirm_reason_for, parse_account_arg, run, run_all};
use crate::agents::{Agent, AgentError, UserContext};

pub struct MeAgent {
    client: GithubClient,
}

impl MeAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

/// Project one `GET /user` payload into the LLM-facing profile row.
fn project_me(raw: &Value) -> Value {
    json!({
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
    })
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
         `author:@me` search queries. Args: `account?` (a name from `github_list_accounts`, \
         `\"default\"`, or `\"all\"` to get every configured account's profile at once). \
         Read-only; requires the GitHub integration to be configured in /settings/integrations."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "account": account_prop(true),
            },
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
        args: &Value,
    ) -> crate::agents::ConfirmationDecision {
        confirm_reason_for(self.name(), parse_account_arg(args).as_deref())
    }

    fn untrusted_output(&self) -> bool {
        true
    }

    async fn invoke(
        &self,
        ctx: &UserContext,
        args: Value,
    ) -> Result<String, crate::agents::AgentError> {
        let account_arg = parse_account_arg(&args);
        if account_arg.as_deref() == Some(nagent_github::ALL_ACCOUNTS) {
            return self.invoke_all(ctx, &args).await;
        }
        let account = self
            .client
            .resolve_account(ctx, account_arg.as_deref())
            .await?;
        let name = account.name.clone();
        run(
            &self.client,
            ctx,
            &account,
            self.name(),
            &args,
            ("/user", Vec::new()),
            move |raw, _cfg| {
                let mut data = serde_json::Map::new();
                data.insert("account".into(), json!(name));
                if let Some(p) = project_me(raw).as_object() {
                    for (k, v) in p {
                        data.insert(k.clone(), v.clone());
                    }
                }
                json!({ "ok": true, "data": Value::Object(data) })
            },
        )
        .await
    }
}

impl MeAgent {
    /// `account: "all"` — one profile row per configured account
    /// shaped `{name, user, plan, scopes}` (name = account name,
    /// user = GitHub login), joined with the stored token-free
    /// scopes metadata. Per-account failures land in
    /// `account_errors`; the call fails only if every account fails.
    async fn invoke_all(&self, ctx: &UserContext, args: &Value) -> Result<String, AgentError> {
        let client = self.client.clone();
        let call = move |acc: super::Account| {
            let client = client.clone();
            async move {
                let raw = client.get_with_account(&acc, "/user", &[]).await?;
                let row = json!({
                    // merge_tagged tags `account`; `name` here is the account name.
                    "id": acc.name,
                    "name": acc.name,
                    "user": raw.get("login"),
                    "plan": raw.get("plan"),
                    "avatar_url": raw.get("avatar_url"),
                    "html_url": raw.get("html_url"),
                    "created_at": raw.get("created_at"),
                });
                Ok((vec![row], Value::Null))
            }
        };
        let (merged, account_errors, _meta) = run_all(
            &self.client,
            ctx,
            self.name(),
            args,
            self.client.cfg.max_items,
            "name",
            call,
        )
        .await?;
        // Join the stored scopes metadata (token-free) per account.
        let metas = accounts::list_accounts(ctx, &self.client.cfg)
            .await
            .unwrap_or_default();
        let scopes: serde_json::Map<String, Value> = metas
            .iter()
            .map(|m| {
                (
                    m.name.clone(),
                    m.scopes.clone().map(Value::String).unwrap_or(Value::Null),
                )
            })
            .collect();
        let accounts_json: Vec<Value> = merged
            .iter()
            .map(|row| {
                let account_name = row.get("account").and_then(|v| v.as_str()).unwrap_or("");
                let mut r = row.clone();
                if let Some(obj) = r.as_object_mut() {
                    obj.remove("id");
                    obj.insert(
                        "scopes".into(),
                        scopes.get(account_name).cloned().unwrap_or(Value::Null),
                    );
                }
                r
            })
            .collect();
        let out = json!({
            "ok": true,
            "data": {
                "account": "all",
                "count": accounts_json.len(),
                "accounts": accounts_json,
                "account_errors": Value::Object(account_errors),
            }
        });
        let encoded = serde_json::to_string(&out)
            .map_err(|e| AgentError::AgentFailed(format!("github encode: {e}")))?;
        self.client.cache_put(
            ctx.user_id(),
            nagent_github::ALL_ACCOUNTS,
            self.name(),
            args,
            encoded.clone(),
        );
        Ok(encoded)
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
    fn schema_has_only_the_account_property() {
        let agent = MeAgent::new(GithubClient::new(
            crate::GithubAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["account"]
        );
    }
}
