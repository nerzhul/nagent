//! `github_list_notifications` — `GET /notifications`.

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{account_prop, confirm_reason_for, parse_account_arg, parse_limit, run, run_all};
use crate::agents::{Agent, AgentError, UserContext};

pub struct ListNotificationsAgent {
    client: GithubClient,
}

impl ListNotificationsAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

fn project_notification(n: &Value) -> Value {
    json!({
        // `id` feeds the `account: "all"` dedupe (merge_tagged).
        "id": n.get("id"),
        "reason": n.get("reason"),
        "unread": n.get("unread"),
        "subject_type": n.get("subject").and_then(|s| s.get("type")),
        "subject_title": n.get("subject").and_then(|s| s.get("title")),
        "subject_url": n.get("subject").and_then(|s| s.get("url")),
        "repository": n.get("repository").and_then(|r| r.get("full_name")),
        "updated_at": n.get("updated_at"),
    })
}

#[async_trait]
impl Agent for ListNotificationsAgent {
    fn name(&self) -> &str {
        "github_list_notifications"
    }

    fn description(&self) -> &str {
        "List the authenticated user's GitHub notifications (`GET /notifications`): inbox \
         items for mentions, reviews, assignments, etc., each with reason, unread flag, \
         subject title/type, repository, and update date. Args: `unread_only?` (default \
         false — pass true to see only unread), `limit?` (default 30, server cap 100), \
         `account?` (a name from `github_list_accounts`, `\"default\"`, or `\"all\"` to \
         merge every configured account's notifications, each tagged with its `account`). \
         Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "unread_only": {
                    "type": "boolean",
                    "default": false,
                    "description": "When true, only unread notifications are returned.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 30,
                },
                "account": account_prop(true),
            },
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "github",
            "notifications",
            "inbox",
            "mentions",
            "unread",
            "alerts",
            "activity",
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
        let limit = parse_limit(&args, &self.client.cfg, 30);
        let unread_only = args
            .get("unread_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut query = vec![("per_page".to_string(), limit.to_string())];
        if !unread_only {
            // `all=true` includes read notifications; the default
            // (absent) is unread-only, so we only pass `all` when
            // the caller wants everything.
            query.push(("all".to_string(), "true".to_string()));
        }
        let account_arg = parse_account_arg(&args);
        if account_arg.as_deref() == Some(nagent_github::ALL_ACCOUNTS) {
            return self.invoke_all(ctx, &args, query, limit, unread_only).await;
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
            ("/notifications", query),
            move |raw, _cfg| {
                let notifications: Vec<Value> = raw
                    .as_array()
                    .map(|arr| arr.iter().map(project_notification).collect())
                    .unwrap_or_default();
                json!({
                    "ok": true,
                    "data": {
                        "account": name,
                        "unread_only": unread_only,
                        "count": notifications.len(),
                        "notifications": notifications,
                    }
                })
            },
        )
        .await
    }
}

impl ListNotificationsAgent {
    /// `account: "all"` — merged notification list across every
    /// configured account: items tagged `"account"`, deduped by
    /// upstream `id`, sorted by `updated_at` desc; per-account
    /// failures collected in `account_errors`.
    async fn invoke_all(
        &self,
        ctx: &UserContext,
        args: &Value,
        query: Vec<(String, String)>,
        limit: usize,
        unread_only: bool,
    ) -> Result<String, AgentError> {
        let client = self.client.clone();
        let call = move |acc: super::Account| {
            let client = client.clone();
            let query = query.clone();
            async move {
                let raw = client
                    .get_with_account(&acc, "/notifications", &query)
                    .await?;
                let items: Vec<Value> = raw
                    .as_array()
                    .map(|arr| arr.iter().map(project_notification).collect())
                    .unwrap_or_default();
                Ok((items, Value::Null))
            }
        };
        let (merged, account_errors, _meta) = run_all(
            &self.client,
            ctx,
            self.name(),
            args,
            limit,
            "updated_at",
            call,
        )
        .await?;
        let out = json!({
            "ok": true,
            "data": {
                "account": "all",
                "unread_only": unread_only,
                "count": merged.len(),
                "notifications": merged,
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
    fn project_notification_flattens_subject_and_repo() {
        let raw = json!({
            "id": "1",
            "reason": "mention",
            "unread": true,
            "subject": {"type": "Issue", "title": "bug", "url": "https://api.github.com/x"},
            "repository": {"full_name": "octo/hello"},
            "updated_at": "2026-11-25T10:00:00Z",
        });
        let p = project_notification(&raw);
        assert_eq!(p["subject_type"], "Issue");
        assert_eq!(p["subject_title"], "bug");
        assert_eq!(p["repository"], "octo/hello");
        assert_eq!(p["unread"], true);
    }
}
