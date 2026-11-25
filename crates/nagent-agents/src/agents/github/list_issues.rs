//! `github_list_issues` — `GET /repos/{owner}/{repo}/issues`.
//!
//! GitHub's issues endpoint also returns pull requests; this
//! tool filters them out (entries carrying a `pull_request`
//! key) so the LLM gets a pure issue list.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{
    cap_text, confirm_reason, label_names, login_of, parse_enum, parse_limit, parse_repo, run,
};
use crate::agents::{Agent, AgentError, UserContext};

const STATES: &[&str] = &["open", "closed", "all"];

pub struct ListIssuesAgent {
    client: GithubClient,
}

impl ListIssuesAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

/// Project one issue into the LLM-facing row. PRs (entries with
/// a `pull_request` key) are skipped by the caller.
fn project_issue(i: &Value, cfg: &crate::GithubAgentConfig) -> Value {
    json!({
        "number": i.get("number"),
        "title": i.get("title"),
        "state": i.get("state"),
        "user": login_of(i),
        "labels": label_names(i),
        "comments": i.get("comments"),
        "created_at": i.get("created_at"),
        "updated_at": i.get("updated_at"),
        "body": cap_text(&i.get("body").cloned().unwrap_or(Value::Null), cfg.max_body_bytes),
        "html_url": i.get("html_url"),
    })
}

#[async_trait]
impl Agent for ListIssuesAgent {
    fn name(&self) -> &str {
        "github_list_issues"
    }

    fn description(&self) -> &str {
        "List issues in a GitHub repository (`GET /repos/{owner}/{repo}/issues`). Pull \
         requests are filtered out. Args: `repo` (`\"owner/repo\"`), `state?` \
         (open/closed/all, default open), `labels?` (comma-separated label names to \
         filter on), `limit?` (default 30, server cap 100). Returns number, title, \
         state, author, labels, comment count, dates, and the capped body. Read-only."
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
                "labels": {
                    "type": "string",
                    "description": "Comma-separated label names; only issues carrying ALL of them are returned.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 30,
                },
            },
            "required": ["repo"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "github", "issues", "tickets", "bugs", "list", "open", "todo",
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
        let limit = parse_limit(&args, &self.client.cfg, 30);
        let state = parse_enum(&args, "state", STATES, "open")?;
        let labels = args
            .get("labels")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let path = format!("/repos/{owner}/{repo}/issues");
        let mut query = vec![
            ("state".to_string(), state),
            ("per_page".to_string(), limit.to_string()),
        ];
        if let Some(labels) = &labels {
            query.push(("labels".to_string(), labels.clone()));
        }
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            &path,
            query,
            move |raw, cfg| {
                let issues: Vec<Value> = raw
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter(|i| i.get("pull_request").is_none())
                            .map(|i| project_issue(i, cfg))
                            .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "ok": true,
                    "data": {
                        "repo": format!("{owner}/{repo}"),
                        "count": issues.len(),
                        "issues": issues,
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
    fn project_issue_caps_body() {
        let cfg = crate::GithubAgentConfig {
            max_body_bytes: 10,
            ..Default::default()
        };
        let raw = json!({
            "number": 7,
            "title": "crash on startup",
            "state": "open",
            "user": {"login": "octo"},
            "labels": [{"name": "bug"}, {"name": "p0"}],
            "comments": 3,
            "body": "x".repeat(500),
            "html_url": "https://github.com/o/r/issues/7"
        });
        let p = project_issue(&raw, &cfg);
        assert_eq!(p["number"], 7);
        assert_eq!(p["user"], "octo");
        assert_eq!(p["labels"], json!(["bug", "p0"]));
        assert!(p["body"].as_str().unwrap().ends_with("…[truncated]"));
    }
}
