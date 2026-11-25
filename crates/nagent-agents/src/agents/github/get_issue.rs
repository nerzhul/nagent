//! `github_get_issue` — `GET /repos/{owner}/{repo}/issues/{n}`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{cap_text, confirm_reason, label_names, login_of, parse_number, parse_repo, run};
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
         Args: `repo` (`\"owner/repo\"`) and `number` (positive integer). Read-only."
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
        _args: &Value,
    ) -> crate::agents::ConfirmationDecision {
        confirm_reason(self.name())
    }

    fn untrusted_output(&self) -> bool {
        true
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let (owner, repo) = parse_repo(&args)?;
        let number = parse_number(&args)?;
        let path = format!("/repos/{owner}/{repo}/issues/{number}");
        run(&self.client, ctx, self.name(), &args, &path, Vec::new(), move |raw, cfg| {
            json!({
                "ok": true,
                "data": {
                    "repo": format!("{owner}/{repo}"),
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
