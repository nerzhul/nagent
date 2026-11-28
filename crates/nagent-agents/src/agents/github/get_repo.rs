//! `github_get_repo` — `GET /repos/{owner}/{repo}`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{
    account_prop, confirm_reason_for, parse_account_arg, parse_repo, resolve_repo_account, run,
};
use crate::agents::{Agent, AgentError, UserContext};

pub struct GetRepoAgent {
    client: GithubClient,
}

impl GetRepoAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Agent for GetRepoAgent {
    fn name(&self) -> &str {
        "github_get_repo"
    }

    fn description(&self) -> &str {
        "Fetch a single GitHub repository's details (`GET /repos/{owner}/{repo}`): \
         description, visibility, default branch, language, stars, watchers, open issues, \
         topics, created/updated/pushed dates, homepage, license. Args: `owner` and `repo` \
         as separate fields (or `repo` as `\"owner/repo\"`), `account?` (a name from \
         `github_list_accounts` or `\"default\"`; omit to auto-resolve the account that \
         can read the repo). Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": "Repo owner (user or organisation login).",
                },
                "repo": {
                    "type": "string",
                    "description": "Repository name, or `\"owner/repo\"`. When it contains a slash, `owner` is optional.",
                },
                "account": account_prop(false),
            },
            "required": ["repo"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "github",
            "repo",
            "repository",
            "details",
            "readme",
            "stars",
            "info",
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

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        // Accept either `repo: "owner/repo"` or `owner` + `repo` separately.
        let (owner, repo) = match args.get("repo").and_then(|v| v.as_str()) {
            Some(r) if r.contains('/') => parse_repo(&args)?,
            _ => {
                let owner = args.get("owner").and_then(|v| v.as_str()).ok_or_else(|| {
                    AgentError::InvalidArguments(
                        "`owner` is required when `repo` is not shaped `\"owner/repo\"`".into(),
                    )
                })?;
                let repo = args
                    .get("repo")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| AgentError::InvalidArguments("`repo` is required".into()))?;
                // Validate the combined shape through the same
                // owner/repo name rules as the `"owner/repo"` form.
                parse_repo(&json!({"repo": format!("{owner}/{repo}")}))?;
                (owner.to_string(), repo.to_string())
            }
        };
        let full = format!("{owner}/{repo}");
        let account = resolve_repo_account(
            &self.client,
            ctx,
            &full,
            parse_account_arg(&args).as_deref(),
        )
        .await?;
        let account_name = account.name.clone();
        let path = format!("/repos/{owner}/{repo}");
        run(
            &self.client,
            ctx,
            &account,
            self.name(),
            &args,
            (path.as_str(), Vec::new()),
            move |raw, _cfg| {
                json!({
                    "ok": true,
                    "data": {
                        "account": account_name,
                        "full_name": raw.get("full_name"),
                        "description": raw.get("description"),
                        "private": raw.get("private"),
                        "visibility": raw.get("visibility"),
                        "fork": raw.get("fork"),
                        "language": raw.get("language"),
                        "topics": raw.get("topics"),
                        "stargazers_count": raw.get("stargazers_count"),
                        "watchers_count": raw.get("watchers_count"),
                        "forks_count": raw.get("forks_count"),
                        "open_issues_count": raw.get("open_issues_count"),
                        "default_branch": raw.get("default_branch"),
                        "homepage": raw.get("homepage"),
                        "license": raw.get("license"),
                        "created_at": raw.get("created_at"),
                        "updated_at": raw.get("updated_at"),
                        "pushed_at": raw.get("pushed_at"),
                        "html_url": raw.get("html_url"),
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
    fn schema_requires_repo() {
        let agent = GetRepoAgent::new(GithubClient::new(
            crate::GithubAgentConfig::default(),
            reqwest::Client::new(),
        ));
        assert_eq!(agent.parameters_schema()["required"], json!(["repo"]));
    }
}
