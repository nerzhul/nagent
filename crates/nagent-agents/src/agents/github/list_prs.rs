//! `github_list_prs` — `GET /repos/{owner}/{repo}/pulls`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{
    account_prop, confirm_reason_for, login_of, parse_account_arg, parse_enum, parse_limit,
    parse_repo, resolve_repo_account, run,
};
use crate::agents::{Agent, AgentError, UserContext};

const STATES: &[&str] = &["open", "closed", "all"];

pub struct ListPrsAgent {
    client: GithubClient,
}

impl ListPrsAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

fn project_pr(p: &Value) -> Value {
    json!({
        "number": p.get("number"),
        "title": p.get("title"),
        "state": p.get("state"),
        "user": login_of(p),
        "draft": p.get("draft"),
        "merged": p.get("merged"),
        "head_ref": p.get("head").and_then(|h| h.get("ref")),
        "base_ref": p.get("base").and_then(|b| b.get("ref")),
        "created_at": p.get("created_at"),
        "updated_at": p.get("updated_at"),
        "merged_at": p.get("merged_at"),
        "html_url": p.get("html_url"),
    })
}

#[async_trait]
impl Agent for ListPrsAgent {
    fn name(&self) -> &str {
        "github_list_prs"
    }

    fn description(&self) -> &str {
        "List pull requests in a GitHub repository (`GET /repos/{owner}/{repo}/pulls`). \
         Args: `repo` (`\"owner/repo\"`), `state?` (open/closed/all, default open), \
         `limit?` (default 30, server cap 100), `account?` (a name from \
         `github_list_accounts` or `\"default\"`; omit to auto-resolve the account that \
         can read the repo). Returns number, title, state, author, \
         draft/merged flags, head/base branches, dates, and URL. Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Repository as `\"owner/repo\"`.",
                },
                "state": {
                    "type": "string",
                    "enum": STATES,
                    "default": "open",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 30,
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
            "pull requests",
            "prs",
            "pr",
            "merge",
            "review",
            "list",
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
        let limit = parse_limit(&args, &self.client.cfg, 30);
        let state = parse_enum(&args, "state", STATES, "open")?;
        let path = format!("/repos/{owner}/{repo}/pulls");
        let query = vec![
            ("state".to_string(), state),
            ("per_page".to_string(), limit.to_string()),
        ];
        run(
            &self.client,
            ctx,
            &account,
            self.name(),
            &args,
            (path.as_str(), query),
            move |raw, _cfg| {
                let prs: Vec<Value> = raw
                    .as_array()
                    .map(|arr| arr.iter().map(project_pr).collect())
                    .unwrap_or_default();
                json!({
                    "ok": true,
                    "data": {
                        "account": account_name,
                        "repo": full,
                        "count": prs.len(),
                        "pull_requests": prs,
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
    fn project_pr_extracts_head_base_refs() {
        let raw = json!({
            "number": 3,
            "title": "fix",
            "state": "open",
            "user": {"login": "octo"},
            "draft": true,
            "head": {"ref": "feature-x"},
            "base": {"ref": "main"},
        });
        let p = project_pr(&raw);
        assert_eq!(p["head_ref"], "feature-x");
        assert_eq!(p["base_ref"], "main");
        assert_eq!(p["draft"], true);
        assert_eq!(p["user"], "octo");
    }
}
