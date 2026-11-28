//! `github_search_issues` — `GET /search/issues` with GitHub
//! search syntax (e.g. `author:@me is:open`, `repo:owner/name
//! is:pr is:open`). The query is passed through verbatim.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{
    account_prop, confirm_reason_for, label_names, login_of, parse_account_arg, parse_limit, run,
    run_all,
};
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
        // `id` feeds the `account: "all"` dedupe (merge_tagged).
        "id": i.get("id"),
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
         (default 30, server cap 100), `account?` (a name from `github_list_accounts`, \
         `\"default\"`, or `\"all\"` to fan the same query out across every configured \
         account and merge the hits, each tagged with its `account`; `total_count` is \
         the sum across accounts). Returns total_count plus matching items. \
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
                "account": account_prop(true),
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
        args: &Value,
    ) -> crate::agents::ConfirmationDecision {
        confirm_reason_for(self.name(), parse_account_arg(args).as_deref())
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
        let account_arg = parse_account_arg(&args);
        if account_arg.as_deref() == Some(nagent_github::ALL_ACCOUNTS) {
            return self.invoke_all(ctx, &args, q, limit).await;
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
            ("/search/issues", q),
            move |raw, _cfg| {
                let items: Vec<Value> = raw
                    .get("items")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().map(project_hit).collect())
                    .unwrap_or_default();
                json!({
                    "ok": true,
                    "data": {
                        "account": name,
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

impl SearchIssuesAgent {
    /// `account: "all"` — the same query fanned out across every
    /// configured account; hits merged, tagged `"account"`, deduped
    /// by upstream `id`, sorted by `updated_at` desc; `total_count`
    /// is the sum of the per-account totals; per-account failures
    /// land in `account_errors`.
    async fn invoke_all(
        &self,
        ctx: &UserContext,
        args: &Value,
        q: Vec<(String, String)>,
        limit: usize,
    ) -> Result<String, AgentError> {
        let client = self.client.clone();
        let call = move |acc: super::Account| {
            let client = client.clone();
            let q = q.clone();
            async move {
                let raw = client.get_with_account(&acc, "/search/issues", &q).await?;
                let items: Vec<Value> = raw
                    .get("items")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().map(project_hit).collect())
                    .unwrap_or_default();
                let meta = json!({ "total_count": raw.get("total_count") });
                Ok((items, meta))
            }
        };
        let (merged, account_errors, account_meta) = run_all(
            &self.client,
            ctx,
            self.name(),
            args,
            limit,
            "updated_at",
            call,
        )
        .await?;
        let total_count: u64 = account_meta
            .values()
            .filter_map(|m| m.get("total_count").and_then(|v| v.as_u64()))
            .sum();
        let out = json!({
            "ok": true,
            "data": {
                "account": "all",
                "total_count": total_count,
                "count": merged.len(),
                "items": merged,
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
