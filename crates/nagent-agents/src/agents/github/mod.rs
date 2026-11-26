//! GitHub agent family (plan 1795597719368) — ten **read-only**
//! chat tools over the GitHub REST API with the calling user's
//! personal access token:
//!
//! | Tool | Endpoint |
//! | --- | --- |
//! | `github_me` | `GET /user` |
//! | `github_list_repos` | `GET /user/repos` or `GET /users/{owner}/repos` |
//! | `github_get_repo` | `GET /repos/{owner}/{repo}` |
//! | `github_list_issues` | `GET /repos/{owner}/{repo}/issues` |
//! | `github_get_issue` | `GET /repos/{owner}/{repo}/issues/{n}` |
//! | `github_list_prs` | `GET /repos/{owner}/{repo}/pulls` |
//! | `github_get_pr` | `GET /repos/{owner}/{repo}/pulls/{n}` |
//! | `github_list_notifications` | `GET /notifications` |
//! | `github_search_issues` | `GET /search/issues` |
//! | `github_read_file` | `GET /repos/{owner}/{repo}/contents/{path}` |
//!
//! All tools share one [`client::GithubClient`] shape (per-tool
//! instance built by the descriptor table, all draining the
//! shared `EgressPool::public()` client) and one short-lived
//! in-process cache keyed by `(user_id, tool, args)` — GitHub
//! PAT quota is 5 000 req/h, so the `cache_ttl_secs` default of
//! 60 s keeps repeated questions cheap.
//!
//! Write tools (`github_create_issue`, `github_create_pr`,
//! `github_comment`) are deliberately out of scope for v1; the
//! descriptor table does not register them, so the LLM tool loop
//! returns "unknown tool" if a model tries.
//!
//! ## Confirmation posture
//!
//! Every tool is read-only but still hits a third-party service
//! with the user's credentials, so every call requires the
//! inline approval card (strict per-call, plan 1791834996882 —
//! no same-turn re-call shortcut). The session-wide "Toujours
//! pour cette session" override is allowed for all of them
//! (the trait default).

pub mod client;
pub mod get_issue;
pub mod get_pr;
pub mod get_repo;
pub mod list_issues;
pub mod list_notifications;
pub mod list_prs;
pub mod list_repos;
pub mod me;
pub mod read_file;
pub mod search_issues;

pub use client::GithubClient;

use serde_json::Value;

use crate::agents::{AgentError, ConfirmationDecision, UserContext};
use crate::GithubAgentConfig;

/// Run one tool end-to-end: cache lookup → authenticated GET →
/// projection → JSON encode → cache store. `project` receives
/// the raw upstream JSON and the agent config and returns the
/// LLM-facing envelope.
pub(crate) async fn run<F>(
    client: &GithubClient,
    ctx: &UserContext,
    tool: &str,
    args: &Value,
    path: &str,
    query: Vec<(String, String)>,
    project: F,
) -> Result<String, AgentError>
where
    F: FnOnce(&Value, &GithubAgentConfig) -> Value,
{
    if let Some(cached) = client.cache_get(ctx.user_id(), tool, args) {
        tracing::debug!(agent = tool, user_id = %ctx.user_id(), "github cache hit");
        return Ok(cached);
    }
    let raw = client.get(ctx, path, &query).await?;
    let out = project(&raw, &client.cfg);
    let encoded = serde_json::to_string(&out)
        .map_err(|e| AgentError::AgentFailed(format!("github encode: {e}")))?;
    client.cache_put(ctx.user_id(), tool, args, encoded.clone());
    Ok(encoded)
}

/// Shared confirmation reason for every `github_*` tool. The
/// reason names the tool so the approval card is specific.
pub(crate) fn confirm_reason(tool: &str) -> ConfirmationDecision {
    ConfirmationDecision::NeedsConfirmation {
        reason: format!(
            "`{tool}` reads the user's GitHub account through the GitHub API with their \
             personal access token. The user must explicitly confirm in chat before this \
             call runs. Wait for the user to click \"Autoriser\" in the chat UI; the tool \
             will execute on their next message and you will see its result then.",
        ),
    }
}

/// Parse the `limit` argument: default `default`, clamped to
/// `[1, cfg.max_items]`.
pub(crate) fn parse_limit(args: &Value, cfg: &GithubAgentConfig, default: usize) -> usize {
    args.get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(default)
        .min(cfg.max_items)
        .max(1)
}

/// Parse an enum-ish string argument against an allow-list.
/// Returns `default` when absent; `InvalidArguments` when the
/// value is not one of `allowed`.
pub(crate) fn parse_enum(
    args: &Value,
    key: &str,
    allowed: &[&str],
    default: &str,
) -> Result<String, AgentError> {
    match args.get(key).and_then(|v| v.as_str()) {
        None => Ok(default.to_string()),
        Some(v) if allowed.contains(&v) => Ok(v.to_string()),
        Some(other) => Err(AgentError::InvalidArguments(format!(
            "`{key}` must be one of {}, got `{other}`",
            allowed.join(", ")
        ))),
    }
}

/// Parse a `repo` argument shaped `"owner/repo"` into its two
/// parts, validating the GitHub name rules with the shared
/// [`nagent_github::valid_repo_full_name`] validator (the same one
/// the dashboard repo filter and the probe-repo endpoint use, so
/// a malformed value is rejected identically everywhere).
pub(crate) fn parse_repo(args: &Value) -> Result<(String, String), AgentError> {
    let raw = args.get("repo").and_then(|v| v.as_str()).ok_or_else(|| {
        AgentError::InvalidArguments("`repo` is required, shaped `\"owner/repo\"`".into())
    })?;
    match nagent_github::valid_repo_full_name(raw) {
        Some((owner, repo)) => Ok((owner.to_string(), repo.to_string())),
        None => {
            if raw.contains('/') {
                Err(AgentError::InvalidArguments(format!(
                    "`repo` has an invalid owner/repo name: `{raw}` (allowed: alphanumeric, \
                     `-`, `_`, `.`)"
                )))
            } else {
                Err(AgentError::InvalidArguments(format!(
                    "`repo` must be shaped `\"owner/repo\"`, got `{raw}`"
                )))
            }
        }
    }
}

/// Parse the `number` argument (issue / PR number): a positive
/// integer.
pub(crate) fn parse_number(args: &Value) -> Result<u64, AgentError> {
    args.get("number")
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            AgentError::InvalidArguments(
                "`number` is required and must be a positive integer".into(),
            )
        })
}

/// Truncate a JSON string field (`body`, file content, …) to
/// `cap` bytes at a char boundary, appending a marker. Returns
/// the original value when it fits.
pub(crate) fn cap_text(value: &Value, cap: usize) -> Value {
    match value {
        Value::String(s) => Value::String(client::truncate(s, cap)),
        other => other.clone(),
    }
}

/// Extract `login` from a GitHub `user` object.
pub(crate) fn login_of(v: &Value) -> Value {
    v.get("user")
        .and_then(|u| u.get("login"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Extract the label names from a GitHub `labels` array.
pub(crate) fn label_names(v: &Value) -> Vec<Value> {
    v.get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| arr.iter().filter_map(|l| l.get("name").cloned()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> GithubAgentConfig {
        GithubAgentConfig::default()
    }

    #[test]
    fn parse_limit_defaults_and_caps() {
        assert_eq!(parse_limit(&json!({}), &cfg(), 30), 30);
        assert_eq!(parse_limit(&json!({"limit": 5}), &cfg(), 30), 5);
        assert_eq!(parse_limit(&json!({"limit": 99999}), &cfg(), 30), 100);
        assert_eq!(parse_limit(&json!({"limit": 0}), &cfg(), 30), 1);
    }

    #[test]
    fn parse_enum_validates_against_allowlist() {
        assert_eq!(
            parse_enum(&json!({}), "state", &["open", "closed", "all"], "open").unwrap(),
            "open"
        );
        assert_eq!(
            parse_enum(
                &json!({"state": "closed"}),
                "state",
                &["open", "closed", "all"],
                "open"
            )
            .unwrap(),
            "closed"
        );
        assert!(parse_enum(
            &json!({"state": "merged"}),
            "state",
            &["open", "closed", "all"],
            "open"
        )
        .is_err());
    }

    #[test]
    fn parse_repo_splits_owner_and_repo() {
        let (o, r) = parse_repo(&json!({"repo": "octo/hello-world"})).unwrap();
        assert_eq!((o.as_str(), r.as_str()), ("octo", "hello-world"));
    }

    #[test]
    fn parse_repo_rejects_malformed_values() {
        for bad in ["noslash", "a/b/c", "../etc", "-bad/repo", "owner/", ""] {
            let err = parse_repo(&json!({"repo": bad})).unwrap_err();
            assert!(
                matches!(err, AgentError::InvalidArguments(_)),
                "expected InvalidArguments for `{bad}`, got {err:?}"
            );
        }
    }

    #[test]
    fn parse_number_requires_positive_integer() {
        assert_eq!(parse_number(&json!({"number": 42})).unwrap(), 42);
        assert!(parse_number(&json!({"number": 0})).is_err());
        assert!(parse_number(&json!({})).is_err());
        assert!(parse_number(&json!({"number": "42"})).is_err());
    }

    #[test]
    fn cap_text_truncates_long_strings() {
        let long = "x".repeat(1000);
        let capped = cap_text(&json!(long), 100);
        assert!(capped.as_str().unwrap().ends_with("…[truncated]"));
        assert_eq!(cap_text(&json!("short"), 100), json!("short"));
        assert_eq!(cap_text(&json!(42), 100), json!(42));
    }
}

/// Loopback HTTP fixture + wire-level tests for the GitHub
/// client: happy path, 404, 401, rate-limit, body cap, base64
/// decode, search-query passthrough, cache, and the AGENTS.md
/// §6 redaction rule (the PAT never appears in an error path).
#[cfg(test)]
mod loopback {
    use super::*;
    use crate::agents::{Agent, SecretSource, UserContext};
    use crate::ServiceRegistry;
    use base64::Engine;
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TEST_TOKEN: &str = "ghp_test_token_0123456789abcdef";

    /// Static in-memory `SecretSource` returning the test PAT.
    struct StaticSecrets;

    #[async_trait::async_trait]
    impl SecretSource for StaticSecrets {
        async fn fetch(
            &self,
            _user_id: uuid::Uuid,
            service: &str,
            field: &str,
        ) -> Result<Option<secrecy::SecretString>, AgentError> {
            if service == "github" && field == "personal_access_token" {
                Ok(Some(secrecy::SecretString::from(TEST_TOKEN)))
            } else {
                Ok(None)
            }
        }
    }

    struct Fixture {
        addr: SocketAddr,
        /// Captured request lines (`GET /path?query HTTP/1.1`)
        /// plus the `Authorization` header value of each request.
        requests: Arc<Mutex<Vec<(String, String)>>>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let requests: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
            let req_clone = requests.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let req_clone = req_clone.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 4096];
                        loop {
                            match sock.read(&mut chunk).await {
                                Ok(0) => break,
                                Ok(n) => {
                                    buf.extend_from_slice(&chunk[..n]);
                                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        let request_line = text.lines().next().unwrap_or("").to_string();
                        let auth = text
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                            .map(|l| {
                                l.split_once(':')
                                    .map(|(_, v)| v.trim().to_string())
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default();
                        req_clone.lock().unwrap().push((request_line.clone(), auth));
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .to_string();
                        let (status, reason, extra, body) = route(&path);
                        let mut resp = format!(
                            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                            body.len()
                        );
                        for (k, v) in extra {
                            resp.push_str(&format!("{k}: {v}\r\n"));
                        }
                        resp.push_str("\r\n");
                        resp.push_str(&body);
                        let _ = sock.write_all(resp.as_bytes()).await;
                        let _ = sock.shutdown().await;
                    });
                }
            });
            Self {
                addr,
                requests,
                _task: task,
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn requests(&self) -> Vec<(String, String)> {
            self.requests.lock().unwrap().clone()
        }
    }

    /// Route table for the fixture. Paths are matched on the
    /// exact request target (path + query).
    fn route(path: &str) -> (u16, &'static str, Vec<(&'static str, String)>, String) {
        let json = |v: Value| v.to_string();
        if path == "/user" {
            return (
                200,
                "OK",
                vec![],
                json(json!({"login": "octo", "name": "Octo Cat", "public_repos": 3})),
            );
        }
        if path.starts_with("/user/repos") {
            return (
                200,
                "OK",
                vec![],
                json(json!([{
                    "full_name": "octo/hello",
                    "description": "demo",
                    "private": false,
                    "stargazers_count": 12,
                    "html_url": "https://github.com/octo/hello"
                }])),
            );
        }
        if path.starts_with("/search/issues") {
            return (
                200,
                "OK",
                vec![],
                json(json!({"total_count": 1, "items": [{
                    "number": 5,
                    "title": "found",
                    "state": "open",
                    "repository_url": "https://api.github.com/repos/octo/hello",
                    "user": {"login": "octo"},
                    "labels": [],
                    "html_url": "https://github.com/octo/hello/issues/5"
                }]})),
            );
        }
        if path.starts_with("/repos/octo/hello/contents/README.md") {
            let content = base64::engine::general_purpose::STANDARD.encode("# Hello\n\nworld\n");
            return (
                200,
                "OK",
                vec![],
                json(json!({
                    "type": "file",
                    "name": "README.md",
                    "path": "README.md",
                    "sha": "abc123",
                    "size": 16,
                    "encoding": "base64",
                    "content": content,
                })),
            );
        }
        if path.starts_with("/repos/octo/hello/issues") {
            return (
                200,
                "OK",
                vec![],
                json(json!([
                    {"number": 1, "title": "issue one", "state": "open",
                     "user": {"login": "octo"}, "labels": [{"name": "bug"}], "body": "x".repeat(500)},
                    {"number": 2, "title": "actually a PR", "pull_request": {"url": "..."}}
                ])),
            );
        }
        if path == "/boom401" {
            return (
                401,
                "Unauthorized",
                vec![],
                json(json!({"message": "Bad credentials"})),
            );
        }
        if path == "/ratelimited" {
            return (
                403,
                "Forbidden",
                vec![
                    ("x-ratelimit-remaining", "0".to_string()),
                    ("retry-after", "30".to_string()),
                ],
                json(json!({"message": "API rate limit exceeded"})),
            );
        }
        (
            404,
            "Not Found",
            vec![],
            json(json!({"message": "Not Found"})),
        )
    }

    fn test_cfg(base_url: String) -> GithubAgentConfig {
        GithubAgentConfig {
            timeout_ms: 5_000,
            max_items: 100,
            max_body_bytes: 64,
            cache_ttl_secs: 60,
            allowlist: vec!["127.0.0.1".to_string()],
            base_url,
        }
    }

    fn test_ctx() -> UserContext {
        UserContext::new(
            uuid::Uuid::new_v4(),
            Arc::new(ServiceRegistry::empty()),
            Arc::new(StaticSecrets),
        )
    }

    #[tokio::test]
    async fn happy_path_get_user_sends_bearer_and_parses() {
        let fx = Fixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let v = client.get(&ctx, "/user", &[]).await.expect("GET /user");
        assert_eq!(v["login"], "octo");
        let reqs = fx.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, "GET /user HTTP/1.1");
        assert_eq!(reqs[0].1, format!("Bearer {TEST_TOKEN}"));
    }

    #[tokio::test]
    async fn not_found_maps_to_upstream_404() {
        let fx = Fixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let err = client
            .get(&ctx, "/repos/octo/missing/contents/x", &[])
            .await
            .unwrap_err();
        match err {
            AgentError::Upstream { status, body } => {
                assert_eq!(status, 404);
                assert!(body.contains("Not Found"));
            }
            other => panic!("expected Upstream 404, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unauthorized_maps_to_reconnect_hint_and_never_leaks_token() {
        let fx = Fixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let err = client.get(&ctx, "/boom401", &[]).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("401"), "expected a 401 hint, got {msg}");
        assert!(
            !msg.contains(TEST_TOKEN),
            "AGENTS.md §6: the PAT must never appear in an error string; got {msg}"
        );
        // The fixture DID receive the token in the header (the
        // auth was actually attempted) — the redaction is about
        // the error path, not about not sending it.
        assert_eq!(fx.requests()[0].1, format!("Bearer {TEST_TOKEN}"));
    }

    #[tokio::test]
    async fn rate_limited_maps_to_rate_limited_with_hint() {
        let fx = Fixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let err = client.get(&ctx, "/ratelimited", &[]).await.unwrap_err();
        match err {
            AgentError::RateLimited { hint } => assert!(hint.contains("30s"), "hint: {hint}"),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cache_avoids_second_upstream_call() {
        let fx = Fixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let args = json!({"limit": 10});
        let first = run(
            &client,
            &ctx,
            "github_list_repos",
            &args,
            "/user/repos",
            vec![],
            |raw, _| raw.clone(),
        )
        .await
        .unwrap();
        let second = run(
            &client,
            &ctx,
            "github_list_repos",
            &args,
            "/user/repos",
            vec![],
            |raw, _| raw.clone(),
        )
        .await
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(fx.requests().len(), 1, "second call must hit the cache");
    }

    #[tokio::test]
    async fn search_query_is_percent_encoded_and_passed_through() {
        let fx = Fixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let args = json!({"query": "author:@me is:open", "limit": 5});
        let out = run(
            &client,
            &ctx,
            "github_search_issues",
            &args,
            "/search/issues",
            vec![
                ("q".to_string(), "author:@me is:open".to_string()),
                ("per_page".to_string(), "5".to_string()),
            ],
            |raw, _| json!({"total": raw.get("total_count"), "items": raw.get("items")}),
        )
        .await
        .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["total"], 1);
        let req_line = &fx.requests()[0].0;
        assert!(
            req_line.contains("q=author%3A%40me+is%3Aopen"),
            "query must be percent-encoded on the wire; got {req_line}"
        );
    }

    #[tokio::test]
    async fn read_file_decodes_base64_and_caps_body() {
        let fx = Fixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let agent = super::read_file::ReadFileAgent::new(client);
        let out = agent
            .invoke(&ctx, json!({"repo": "octo/hello", "path": "README.md"}))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["encoding"], "base64");
        // max_body_bytes = 64 in the test cfg; the decoded content
        // is "# Hello\n\nworld\n" (16 bytes) so it fits — bump the
        // cap test to a smaller cfg below.
        assert_eq!(parsed["data"]["truncated"], false);
        assert_eq!(parsed["data"]["content"], "# Hello\n\nworld\n");
    }

    #[tokio::test]
    async fn read_file_truncates_at_max_body_bytes() {
        let fx = Fixture::start().await;
        let mut cfg = test_cfg(fx.base_url());
        cfg.max_body_bytes = 5;
        let client = GithubClient::new(cfg, reqwest::Client::new());
        let ctx = test_ctx();
        let agent = super::read_file::ReadFileAgent::new(client);
        let out = agent
            .invoke(&ctx, json!({"repo": "octo/hello", "path": "README.md"}))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["data"]["truncated"], true);
        assert!(parsed["data"]["content"]
            .as_str()
            .unwrap()
            .ends_with("…[truncated]"));
    }

    #[tokio::test]
    async fn list_issues_filters_pull_requests_and_caps_body() {
        let fx = Fixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let agent = super::list_issues::ListIssuesAgent::new(client);
        let out = agent
            .invoke(&ctx, json!({"repo": "octo/hello", "state": "open"}))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed["data"]["count"], 1,
            "the PR entry must be filtered out"
        );
        let issue = &parsed["data"]["issues"][0];
        assert_eq!(issue["number"], 1);
        assert_eq!(issue["labels"], json!(["bug"]));
        assert!(issue["body"].as_str().unwrap().ends_with("…[truncated]"));
    }
}
