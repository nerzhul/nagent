//! `github_get_pr` — `GET /repos/{owner}/{repo}/pulls/{n}`.
//! The PR body is included, capped at `[agents.github].max_body_bytes`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{cap_text, confirm_reason, login_of, parse_number, parse_repo, run};
use crate::agents::{Agent, AgentError, UserContext};

pub struct GetPrAgent {
    client: GithubClient,
}

impl GetPrAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for GetPrAgent {
    fn name(&self) -> &str {
        "github_get_pr"
    }

    fn description(&self) -> &str {
        "Fetch one GitHub pull request in full (`GET /repos/{owner}/{repo}/pulls/{n}`): \
         title, state, author, draft/merged flags, head/base branches and SHAs, the \
         capped body, additions/deletions, changed files count, review comments count, \
         dates, and URL. Args: `repo` (`\"owner/repo\"`) and `number`. Read-only."
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
                    "description": "Pull request number.",
                },
            },
            "required": ["repo", "number"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "github",
            "pull request",
            "pr",
            "diff",
            "review",
            "body",
            "get",
            "changes",
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
        let (owner, repo) = parse_repo(&args)?;
        let number = parse_number(&args)?;
        let path = format!("/repos/{owner}/{repo}/pulls/{number}");
        run(&self.client, ctx, self.name(), &args, &path, Vec::new(), move |raw, cfg| {
            json!({
                "ok": true,
                "data": {
                    "repo": format!("{owner}/{repo}"),
                    "number": raw.get("number"),
                    "title": raw.get("title"),
                    "state": raw.get("state"),
                    "user": login_of(raw),
                    "draft": raw.get("draft"),
                    "merged": raw.get("merged"),
                    "head": raw.get("head"),
                    "base": raw.get("base"),
                    "additions": raw.get("additions"),
                    "deletions": raw.get("deletions"),
                    "changed_files": raw.get("changed_files"),
                    "review_comments": raw.get("review_comments"),
                    "comments": raw.get("comments"),
                    "created_at": raw.get("created_at"),
                    "updated_at": raw.get("updated_at"),
                    "merged_at": raw.get("merged_at"),
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
    fn name_is_stable() {
        let agent = GetPrAgent::new(GithubClient::new(
            crate::GithubAgentConfig::default(),
            reqwest::Client::new(),
        ));
        assert_eq!(agent.name(), "github_get_pr");
    }
}
