//! `github_search_issues` — `GET /search/issues` with GitHub
//! search syntax (e.g. `author:@me is:open`, `repo:owner/name
//! is:pr is:open`). The query is passed through verbatim.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{confirm_reason, label_names, login_of, parse_limit, run};
use crate::agents::{Agent, AgentError, UserContext};

pub struct SearchIssuesAgent {
    client: GithubClient,
}

impl SearchIssuesAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

fn project_hit(i: &Value) -> Value {
    // `repository_url` is `https://api.github.com/repos/owner/repo`;
    // derive the `owner/repo` slug from it.
    let repo_slug = i
        .get("repository_url")
        .and_then(|v| v.as_str())
        .and_then(|u| u.rsplit("/repos/").next())
        .unwrap_or("")
        .to_string();
    json!({
        "number": i.get("number"),
        "title": i.get("title"),
        "state": i.get("state"),
        "repo": repo_slug,
        "user": login_of(i),
        "labels": label_names(i),
        "comments": i.get("comments"),
        "created_at": i.get("created_at"),
        "updated_at": i.get("updated_at"),
        "html_url": i.get("html_url"),
    })
}

#[async_trait]
impl Agent for SearchIssuesAgent {
    fn name(&self) -> &str {
        "github_search_issues"
    }

    fn description(&self) -> &str {
        "Search GitHub issues AND pull requests with GitHub search syntax \
         (`GET /search/issues?q=...`). The `query` uses qualifiers like `author:@me`, \
         `is:open`, `is:pr`, `repo:owner/name`, `label:bug`, `created:>2026-01-01`. \
         `@me` refers to the connected user. Args: `query` (required), `limit?` \
         (default 30, server cap 100). Returns total_count plus matching items. \
         Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "GitHub issue/PR search query, e.g. `author:@me is:open` or `repo:octo/hello is:pr is:merged`.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 30,
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "github",
            "search",
            "issues",
            "prs",
            "find",
            "query",
            "author:@me",
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
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AgentError::InvalidArguments(
                    "`query` is required (GitHub search syntax, e.g. `author:@me is:open`)".into(),
                )
            })?
            .to_string();
        let limit = parse_limit(&args, &self.client.cfg, 30);
        let q = vec![
            ("q".to_string(), query),
            ("per_page".to_string(), limit.to_string()),
        ];
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            "/search/issues",
            q,
            |raw, _cfg| {
                let items: Vec<Value> = raw
                    .get("items")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().map(project_hit).collect())
                    .unwrap_or_default();
                json!({
                    "ok": true,
                    "data": {
                        "total_count": raw.get("total_count"),
                        "count": items.len(),
                        "items": items,
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
    fn project_hit_derives_repo_slug_from_repository_url() {
        let raw = json!({
            "number": 12,
            "title": "t",
            "state": "open",
            "repository_url": "https://api.github.com/repos/octo/hello",
            "user": {"login": "octo"},
            "labels": [],
            "html_url": "https://github.com/octo/hello/issues/12"
        });
        let p = project_hit(&raw);
        assert_eq!(p["repo"], "octo/hello");
        assert_eq!(p["number"], 12);
    }
}
