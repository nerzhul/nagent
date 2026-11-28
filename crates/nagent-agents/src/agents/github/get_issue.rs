//! `github_get_issue` — `GET /repos/{owner}/{repo}/issues/{n}`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{
    account_prop, cap_text, confirm_reason_for, label_names, login_of, parse_account_arg,
    parse_number, parse_repo, resolve_repo_account, run,
};
use crate::agents::{Agent, AgentError, UserContext};

pub struct GetIssueAgent {
    client: GithubClient,
}

impl GetIssueAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for GetIssueAgent {
    fn name(&self) -> &str {
        "github_get_issue"
    }

    fn description(&self) -> &str {
        "Fetch one GitHub issue in full (`GET /repos/{owner}/{repo}/issues/{n}`): title, \
         state, author, labels, comment count, the capped body, and the timeline URL. \
         Args: `repo` (`\"owner/repo\"`), `number` (positive integer), `account?` (a name \
         from `github_list_accounts` or `\"default\"`; omit to auto-resolve the account \
         that can read the repo). Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Repository as `\"owner/repo\"`.",
                },
                "number": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Issue number.",
                },
                "account": account_prop(false),
            },
            "required": ["repo", "number"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &["github", "issue", "ticket", "bug", "detail", "body", "get"]
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

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let (owner, repo) = parse_repo(&args)?;
        let full = format!("{owner}/{repo}");
        let account = resolve_repo_account(
            &self.client,
            ctx,
            &full,
            parse_account_arg(&args).as_deref(),
        )
        .await?;
        let account_name = account.name.clone();
        let number = parse_number(&args)?;
        let path = format!("/repos/{owner}/{repo}/issues/{number}");
        run(&self.client, ctx, &account, self.name(), &args, (path.as_str(), Vec::new()), move |raw, cfg| {
            json!({
                "ok": true,
                "data": {
                    "account": account_name,
                    "repo": full,
                    "number": raw.get("number"),
                    "title": raw.get("title"),
                    "state": raw.get("state"),
                    "user": login_of(raw),
                    "labels": label_names(raw),
                    "comments": raw.get("comments"),
                    "created_at": raw.get("created_at"),
                    "updated_at": raw.get("updated_at"),
                    "closed_at": raw.get("closed_at"),
                    "body": cap_text(&raw.get("body").cloned().unwrap_or(Value::Null), cfg.max_body_bytes),
                    "html_url": raw.get("html_url"),
                }
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_requires_repo_and_number() {
        let agent = GetIssueAgent::new(GithubClient::new(
            crate::GithubAgentConfig::default(),
            reqwest::Client::new(),
        ));
        assert_eq!(
            agent.parameters_schema()["required"],
            json!(["repo", "number"])
        );
    }
}
