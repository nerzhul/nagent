//! `github_list_repos` — `GET /user/repos` (or
//! `GET /users/{owner}/repos` when `owner` is given).

use async_trait::async_trait;
use serde_json::{json, Value};

use super::client::GithubClient;
use super::{confirm_reason, parse_enum, parse_limit, run};
use crate::agents::{Agent, AgentError, UserContext};

const TYPES: &[&str] = &["all", "owner", "member"];
const SORTS: &[&str] = &["updated", "pushed", "full_name", "created"];

pub struct ListReposAgent {
    client: GithubClient,
}

impl ListReposAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

/// Project one repo object into the LLM-facing row.
fn project_repo(r: &Value) -> Value {
    json!({
        "full_name": r.get("full_name"),
        "description": r.get("description"),
        "private": r.get("private"),
        "fork": r.get("fork"),
        "language": r.get("language"),
        "stargazers_count": r.get("stargazers_count"),
        "open_issues_count": r.get("open_issues_count"),
        "default_branch": r.get("default_branch"),
        "pushed_at": r.get("pushed_at"),
        "updated_at": r.get("updated_at"),
        "html_url": r.get("html_url"),
    })
}

#[async_trait]
impl Agent for ListReposAgent {
    fn name(&self) -> &str {
        "github_list_repos"
    }

    fn description(&self) -> &str {
        "List GitHub repositories visible to the connected account (`GET /user/repos`), or \
         another user's / organisation's public repos when `owner` is given \
         (`GET /users/{owner}/repos`). Args: `owner?` (omit for the authenticated user), \
         `type?` (all/owner/member, default all), `sort?` (updated/pushed/full_name/created, \
         default updated), `limit?` (default 30, server cap 100). Returns the repo list \
         with full_name, description, visibility, language, stars, open issues, default \
         branch, and last push. Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "owner": {
                    "type": "string",
                    "description": "GitHub login to list repos for. Omit to list the authenticated user's own repos.",
                },
                "type": {
                    "type": "string",
                    "enum": TYPES,
                    "default": "all",
                    "description": "Repo type filter.",
                },
                "sort": {
                    "type": "string",
                    "enum": SORTS,
                    "default": "updated",
                    "description": "Sort field.",
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "default": 30,
                    "description": "Maximum number of repos to return. Server cap is [agents.github].max_items.",
                },
            },
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "github",
            "repos",
            "repositories",
            "list",
            "projects",
            "code",
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
        let limit = parse_limit(&args, &self.client.cfg, 30);
        let typ = parse_enum(&args, "type", TYPES, "all")?;
        let sort = parse_enum(&args, "sort", SORTS, "updated")?;
        let owner = args
            .get("owner")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let (path, mut query) = match &owner {
            Some(o) => (
                format!("/users/{o}/repos"),
                vec![
                    ("type".to_string(), typ.clone()),
                    ("sort".to_string(), sort.clone()),
                    ("per_page".to_string(), limit.to_string()),
                ],
            ),
            None => (
                "/user/repos".to_string(),
                vec![
                    ("type".to_string(), typ.clone()),
                    ("sort".to_string(), sort.clone()),
                    ("per_page".to_string(), limit.to_string()),
                ],
            ),
        };
        query.push(("direction".to_string(), "desc".to_string()));
        run(
            &self.client,
            ctx,
            self.name(),
            &args,
            &path,
            query,
            move |raw, _cfg| {
                let repos: Vec<Value> = raw
                    .as_array()
                    .map(|arr| arr.iter().map(project_repo).collect())
                    .unwrap_or_default();
                json!({
                    "ok": true,
                    "data": {
                        "owner": owner,
                        "count": repos.len(),
                        "repos": repos,
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
    use serde_json::json;

    #[test]
    fn schema_locks_enums_and_defaults() {
        let agent = ListReposAgent::new(GithubClient::new(
            crate::GithubAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let schema = agent.parameters_schema();
        assert_eq!(schema["properties"]["type"]["enum"], json!(TYPES));
        assert_eq!(schema["properties"]["sort"]["enum"], json!(SORTS));
        assert_eq!(schema["properties"]["limit"]["default"], 30);
        assert_eq!(schema["properties"]["limit"]["maximum"], 100);
    }

    #[test]
    fn project_repo_picks_llm_fields() {
        let raw = json!({
            "full_name": "octo/hello",
            "description": "demo",
            "private": false,
            "stargazers_count": 12,
            "html_url": "https://github.com/octo/hello",
            "extra_field": "dropped"
        });
        let p = project_repo(&raw);
        assert_eq!(p["full_name"], "octo/hello");
        assert_eq!(p["stargazers_count"], 12);
        assert!(p.get("extra_field").is_none());
    }
}
