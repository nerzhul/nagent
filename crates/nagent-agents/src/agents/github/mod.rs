//! GitHub agent family (plan 1795597719368) — eleven **read-only**
//! chat tools over the GitHub REST API with the calling user's
//! personal access token(s):
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
//! | `github_list_accounts` | — (vault metadata, no network) |
//!
//! ## Multi-account (plan 1795793328177)
//!
//! Every tool gains an optional **`account`** argument: a name from
//! `github.accounts`, `"default"` (the `personal_access_token`), or
//! `"all"` on the four account-scoped tools (`github_me`,
//! `github_list_repos`, `github_list_notifications`,
//! `github_search_issues`) to fan out across every configured
//! account and merge the results (items tagged `"account"`, deduped
//! by upstream `id`, per-account failures collected in
//! `account_errors`). Repo-scoped tools resolve repo → account via
//! the `github.repo_accounts` map, then live probing (first non-404
//! account wins, cached). `github_list_accounts` lists the
//! token-free account metadata so the LLM can discover accounts.
//!
//! All tools share one [`client::GithubClient`] shape (per-tool
//! instance built by the descriptor table, all draining the
//! shared `EgressPool::public()` client) and one short-lived
//! in-process cache keyed by `(user_id, account, tool, args)` —
//! the account is part of the key so two tokens never share an
//! entry. GitHub PAT quota is 5 000 req/h per token, so the
//! `cache_ttl_secs` default of 60 s keeps repeated questions cheap.
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
//! (the trait default). The confirmation card names the resolved
//! account(s) — never the token.

pub mod accounts;
pub mod client;
pub mod get_issue;
pub mod get_pr;
pub mod get_repo;
pub mod list_accounts;
pub mod list_issues;
pub mod list_notifications;
pub mod list_prs;
pub mod list_repos;
pub mod me;
pub mod read_file;
pub mod search_issues;

pub use accounts::Account;
pub use client::GithubClient;

use std::future::Future;
use std::sync::Arc;

use serde_json::Value;

use crate::agents::{AgentError, ConfirmationDecision, UserContext};
use crate::GithubAgentConfig;

/// Run one tool end-to-end for one resolved account: cache lookup
/// → authenticated GET → projection → JSON encode → cache store.
/// `project` receives the raw upstream JSON and the agent config
/// and returns the LLM-facing envelope. The cache key is
/// `(user_id, account_name, tool, args)` (plan §1.3).
pub(crate) async fn run<F>(
    client: &GithubClient,
    ctx: &UserContext,
    account: &Account,
    tool: &str,
    args: &Value,
    endpoint: (&str, Vec<(String, String)>),
    project: F,
) -> Result<String, AgentError>
where
    F: FnOnce(&Value, &GithubAgentConfig) -> Value,
{
    let (path, query) = endpoint;
    if let Some(cached) = client.cache_get(ctx.user_id(), &account.name, tool, args) {
        tracing::debug!(agent = tool, account = %account.name, user_id = %ctx.user_id(), "github cache hit");
        return Ok(cached);
    }
    let raw = client.get_with_account(account, path, &query).await?;
    let out = project(&raw, &client.cfg);
    let encoded = serde_json::to_string(&out)
        .map_err(|e| AgentError::AgentFailed(format!("github encode: {e}")))?;
    client.cache_put(ctx.user_id(), &account.name, tool, args, encoded.clone());
    Ok(encoded)
}

/// Parse the optional `account` argument: `None`, a name, or
/// `"all"`. Whitespace-trimmed; empty string counts as absent.
pub(crate) fn parse_account_arg(args: &Value) -> Option<String> {
    args.get("account")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The shared `account` schema property added to every tool
/// (plan §2). `aggregates` enables the `"all"` fan-out value in the
/// description (only the four account-scoped tools aggregate).
pub(crate) fn account_prop(aggregates: bool) -> Value {
    let mut desc = "Which configured GitHub account to use: a name from `github_list_accounts`, or `\"default\"` (the personal access token). Omit for the default account.".to_string();
    if aggregates {
        desc.push_str(" `\"all\"` fans out across every configured account and merges the results (items tagged with their account).");
    }
    serde_json::json!({
        "type": "string",
        "description": desc,
    })
}

/// Resolve the account for a **repo-scoped** tool (plan §2.3):
///
/// 1. explicit `account` arg (not `"all"` — repo-scoped tools do
///    not fan out) → resolve by name;
/// 2. the persisted `github.repo_accounts` vault map, else the
///    in-memory map, else live probing of every account (first
///    non-404 wins; positives and negatives cached in memory for
///    `cache_ttl_secs`).
///
/// All-404 surfaces as an `Upstream 404` whose body lists the
/// account **names** tried — never tokens.
pub(crate) async fn resolve_repo_account(
    client: &GithubClient,
    ctx: &UserContext,
    repo: &str,
    account_arg: Option<&str>,
) -> Result<Account, AgentError> {
    if let Some(name) = account_arg.filter(|s| *s != nagent_github::ALL_ACCOUNTS) {
        return client.resolve_account(ctx, Some(name)).await;
    }
    let persisted: Option<std::collections::HashMap<String, String>> = match ctx
        .secret("github", "repo_accounts")
        .await
    {
        Ok(Some(blob)) => {
            use secrecy::ExposeSecret;
            serde_json::from_str::<std::collections::HashMap<String, String>>(blob.expose_secret())
                .ok()
        }
        _ => None,
    };
    let accounts = client.all_accounts(ctx).await?;
    let refs: Vec<nagent_github::AccountToken<'_>> = accounts
        .iter()
        .map(|a| nagent_github::AccountToken {
            name: &a.name,
            token: &a.token,
        })
        .collect();
    let (name, token) =
        nagent_github::resolve_repo_account(client, &refs, ctx.user_id(), repo, persisted.as_ref())
            .await
            .map_err(AgentError::from)?;
    Ok(Account { name, token })
}

/// `account: "all"` fan-out for the four account-scoped tools (plan
/// §2.2): run `call` concurrently (bounded by
/// `aggregate_concurrency`) across every configured account, merge
/// the per-account item lists with [`nagent_github::merge_tagged`]
/// (tagged `"account"`, deduped by upstream `id`, sorted by
/// `sort_key` desc), truncate to `limit`, and collect per-account
/// failures into an `account_errors` object. The call fails only
/// when **every** account fails. The merged envelope is cached
/// under the synthetic account name `"all"`.
///
/// `call` receives the resolved account and returns that account's
/// item list (already projected per-item by the caller's closure)
/// plus a per-account metadata value (e.g. search `total_count`).
/// Per-account item lists are cached under
/// `(user, account, "<tool>#items", args)`.
pub(crate) async fn run_all<Call, Fut>(
    client: &GithubClient,
    ctx: &UserContext,
    tool: &str,
    args: &Value,
    limit: usize,
    sort_key: &str,
    call: Call,
) -> Result<
    (
        Vec<Value>,
        serde_json::Map<String, Value>,
        serde_json::Map<String, Value>,
    ),
    AgentError,
>
where
    Call: Fn(Account) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(Vec<Value>, Value), AgentError>> + Send + 'static,
{
    let accounts = client.all_accounts(ctx).await?;
    let sem = Arc::new(tokio::sync::Semaphore::new(
        client.cfg.aggregate_concurrency.max(1),
    ));
    let client = client.clone();
    let call = Arc::new(call);
    let user_id = ctx.user_id();
    let mut set = tokio::task::JoinSet::new();
    for acc in accounts {
        let sem = sem.clone();
        let client = client.clone();
        let call = call.clone();
        let items_tool = format!("{tool}#items");
        let args = args.clone();
        set.spawn(async move {
            let _permit = sem.acquire_owned().await;
            // Per-account item cache (distinct namespace from the
            // single-account envelope cache).
            if let Some(cached) = client.cache_get(user_id, &acc.name, &items_tool, &args) {
                return match serde_json::from_str::<Vec<Value>>(&cached) {
                    Ok(items) => Ok((acc.name.clone(), items, Value::Null)),
                    Err(e) => Err((
                        acc.name.clone(),
                        AgentError::AgentFailed(format!("github cache decode: {e}")),
                    )),
                };
            }
            match call(acc.clone()).await {
                Ok((items, meta)) => {
                    if let Ok(encoded) = serde_json::to_string(&items) {
                        client.cache_put(user_id, &acc.name, &items_tool, &args, encoded);
                    }
                    Ok((acc.name, items, meta))
                }
                Err(e) => Err((acc.name, e)),
            }
        });
    }
    let mut per_account: Vec<(String, Vec<Value>)> = Vec::new();
    let mut account_errors = serde_json::Map::new();
    let mut account_meta = serde_json::Map::new();
    let mut first_err: Option<AgentError> = None;
    while let Some(res) = set.join_next().await {
        match res {
            Ok(Ok((name, items, meta))) => {
                if !meta.is_null() {
                    account_meta.insert(name.clone(), meta);
                }
                per_account.push((name, items))
            }
            Ok(Err((name, e))) => {
                // AgentError's Display never carries a token
                // (AGENTS.md §6): Upstream bodies are server
                // replies, all other variants are static hints.
                account_errors.insert(name, Value::String(e.to_string()));
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(join_err) => {
                let msg = format!("github fan-out task failed: {join_err}");
                account_errors.insert("<internal>".to_string(), Value::String(msg.clone()));
                if first_err.is_none() {
                    first_err = Some(AgentError::AgentFailed(msg));
                }
            }
        }
    }
    if per_account.is_empty() {
        return Err(first_err.unwrap_or_else(|| {
            AgentError::AgentFailed("github fan-out: no account configured".into())
        }));
    }
    let mut merged = nagent_github::merge_tagged(per_account, sort_key);
    merged.truncate(limit.max(1));
    Ok((merged, account_errors, account_meta))
}

/// Shared confirmation reason for every `github_*` tool. The
/// reason names the tool and the resolved account(s) so the
/// approval card is specific — never the token.
pub(crate) fn confirm_reason(tool: &str) -> ConfirmationDecision {
    confirm_reason_for(tool, None)
}

/// Confirmation reason naming the account selection (`Some` =
/// explicit name or `"all"`).
pub(crate) fn confirm_reason_for(tool: &str, account: Option<&str>) -> ConfirmationDecision {
    let target = match account {
        None => "the default GitHub account".to_string(),
        Some(nagent_github::ALL_ACCOUNTS) => "every configured GitHub account".to_string(),
        Some(name) => format!("the GitHub account `{name}`"),
    };
    ConfirmationDecision::NeedsConfirmation {
        reason: format!(
            "`{tool}` reads the user's GitHub data ({target}) through the GitHub API with \
             their personal access token. The user must explicitly confirm in chat before \
             this call runs. Wait for the user to click \"Autoriser\" in the chat UI; the \
             tool will execute on their next message and you will see its result then.",
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
        if path == "/repos/octo/hello" {
            // Repo-exists route used by the repo→account resolution.
            return (
                200,
                "OK",
                vec![],
                json(json!({
                    "full_name": "octo/hello",
                    "private": false,
                    "default_branch": "main",
                    "html_url": "https://github.com/octo/hello"
                })),
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
            ..Default::default()
        }
    }

    fn default_account() -> Account {
        Account {
            name: nagent_github::DEFAULT_ACCOUNT_NAME.to_string(),
            token: secrecy::SecretString::from(TEST_TOKEN),
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
        let account = default_account();
        let first = run(
            &client,
            &ctx,
            &account,
            "github_list_repos",
            &args,
            ("/user/repos", vec![]),
            |raw, _| raw.clone(),
        )
        .await
        .unwrap();
        let second = run(
            &client,
            &ctx,
            &account,
            "github_list_repos",
            &args,
            ("/user/repos", vec![]),
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
        let account = default_account();
        let out = run(
            &client,
            &ctx,
            &account,
            "github_search_issues",
            &args,
            (
                "/search/issues",
                vec![
                    ("q".to_string(), "author:@me is:open".to_string()),
                    ("per_page".to_string(), "5".to_string()),
                ],
            ),
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

    // ------------------------------------------------------------------
    // Multi-account loopback (plan 1795793328177 §8): a fixture that
    // answers differently per `Authorization` header, so each token
    // maps to its own persona. Asserts the fan-out hits each account
    // with its own token, per-account 401s land in `account_errors`
    // without failing the call, merged results are tagged + deduped,
    // and repo→account resolution caches (one 404 per account per
    // repo).
    // ------------------------------------------------------------------

    const TOKEN_A: &str = "ghp_fanout_token_a_aaaaaaaaaaaa";
    const TOKEN_BAD: &str = "ghp_fanout_token_bad_cccccccccc";

    /// `SecretSource` with a default PAT + two named accounts
    /// (one of them a bad token that the fixture 401s).
    struct MultiSecrets;

    #[async_trait::async_trait]
    impl SecretSource for MultiSecrets {
        async fn fetch(
            &self,
            _user_id: uuid::Uuid,
            service: &str,
            field: &str,
        ) -> Result<Option<secrecy::SecretString>, AgentError> {
            if service != "github" {
                return Ok(None);
            }
            let v = match field {
                "personal_access_token" => Some(TEST_TOKEN.to_string()),
                "accounts" => Some(format!(
                    "[{{\"name\":\"client-a\",\"token\":\"{TOKEN_A}\",\"username\":\"alice\",\"scopes\":\"repo\"}},\
                      {{\"name\":\"client-bad\",\"token\":\"{TOKEN_BAD}\"}}]"
                )),
                _ => None,
            };
            Ok(v.map(secrecy::SecretString::from))
        }
    }

    /// Loopback fixture routing on the `Authorization` header:
    /// - `Bearer {TEST_TOKEN}` (default) → user "octo", repos
    ///   [shared(id 1), own-only(id 2)];
    /// - `Bearer {TOKEN_A}` (client-a) → user "alice", repos
    ///   [shared(id 1, different push date), client-only(id 3)];
    /// - `Bearer {TOKEN_BAD}` → 401;
    /// - repo `org-x/secret` is readable ONLY by client-a (default
    ///   and bad tokens get 404/401).
    struct MultiFixture {
        addr: SocketAddr,
        requests: Arc<Mutex<Vec<(String, String)>>>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl MultiFixture {
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
                        req_clone
                            .lock()
                            .unwrap()
                            .push((request_line.clone(), auth.clone()));
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .to_string();
                        let (status, reason, body) = multi_route(&path, &auth);
                        let resp = format!(
                            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        );
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

    fn multi_route(path: &str, auth: &str) -> (u16, &'static str, String) {
        let json = |v: Value| v.to_string();
        // Bad token → 401 on everything.
        if auth == format!("Bearer {TOKEN_BAD}") {
            return (
                401,
                "Unauthorized",
                json(json!({"message": "Bad credentials"})),
            );
        }
        let (login, repos): (&str, Value) = if auth == format!("Bearer {TOKEN_A}") {
            (
                "alice",
                json!([
                    {"id": 1, "full_name": "shared/repo", "pushed_at": "2026-02-01T00:00:00Z", "html_url": "https://github.com/shared/repo"},
                    {"id": 3, "full_name": "org-x/client-only", "pushed_at": "2026-02-15T00:00:00Z", "html_url": "https://github.com/org-x/client-only"}
                ]),
            )
        } else {
            (
                "octo",
                json!([
                    {"id": 1, "full_name": "shared/repo", "pushed_at": "2026-01-01T00:00:00Z", "html_url": "https://github.com/shared/repo"},
                    {"id": 2, "full_name": "octo/own-only", "pushed_at": "2026-03-01T00:00:00Z", "html_url": "https://github.com/octo/own-only"}
                ]),
            )
        };
        if path == "/user" {
            return (
                200,
                "OK",
                json(json!({"login": login, "plan": {"name": "free"}})),
            );
        }
        if path.starts_with("/user/repos") {
            return (200, "OK", json(repos));
        }
        if path == "/repos/org-x/secret" {
            // Only client-a can read it.
            return if auth == format!("Bearer {TOKEN_A}") {
                (
                    200,
                    "OK",
                    json(json!({"full_name": "org-x/secret", "private": true})),
                )
            } else {
                (404, "Not Found", json(json!({"message": "Not Found"})))
            };
        }
        (404, "Not Found", json(json!({"message": "Not Found"})))
    }

    fn multi_ctx() -> UserContext {
        UserContext::new(
            uuid::Uuid::new_v4(),
            Arc::new(ServiceRegistry::empty()),
            Arc::new(MultiSecrets),
        )
    }

    #[tokio::test]
    async fn fanout_all_hits_each_account_with_its_own_token_and_merges() {
        let fx = MultiFixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = multi_ctx();
        let agent = super::list_repos::ListReposAgent::new(client);
        let out = agent
            .invoke(&ctx, json!({"account": "all", "limit": 30}))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["ok"], true);
        let repos = parsed["data"]["repos"].as_array().unwrap();
        // 3 unique ids (1 shared deduped, 2, 3) — sorted by pushed_at desc.
        assert_eq!(repos.len(), 3, "dedupe by upstream id: {repos:?}");
        assert_eq!(repos[0]["full_name"], "octo/own-only");
        assert_eq!(repos[0]["account"], "default");
        assert_eq!(repos[1]["full_name"], "org-x/client-only");
        assert_eq!(repos[1]["account"], "client-a");
        assert_eq!(repos[2]["full_name"], "shared/repo");
        // shared/repo keeps the FIRST account occurrence (default).
        assert_eq!(repos[2]["account"], "default");
        assert_eq!(repos[2]["pushed_at"], "2026-01-01T00:00:00Z");
        // The bad account's 401 landed in account_errors, not a failure.
        let errs = parsed["data"]["account_errors"].as_object().unwrap();
        assert!(errs.contains_key("client-bad"), "got {errs:?}");
        assert!(
            !out.contains(TOKEN_BAD) && !out.contains(TOKEN_A) && !out.contains(TEST_TOKEN),
            "AGENTS.md §6: no token may appear in the response; got {out}"
        );
        // Each account was called with its own token.
        let reqs = fx.requests();
        let auths: Vec<&String> = reqs.iter().map(|(_, a)| a).collect();
        assert!(auths.contains(&&format!("Bearer {TEST_TOKEN}")));
        assert!(auths.contains(&&format!("Bearer {TOKEN_A}")));
        assert!(auths.contains(&&format!("Bearer {TOKEN_BAD}")));
    }

    #[tokio::test]
    async fn fanout_all_fails_only_when_every_account_fails() {
        struct OnlyBad;
        #[async_trait::async_trait]
        impl SecretSource for OnlyBad {
            async fn fetch(
                &self,
                _u: uuid::Uuid,
                service: &str,
                field: &str,
            ) -> Result<Option<secrecy::SecretString>, AgentError> {
                if service == "github" && field == "personal_access_token" {
                    Ok(Some(secrecy::SecretString::from(TOKEN_BAD)))
                } else {
                    Ok(None)
                }
            }
        }
        let fx = MultiFixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = UserContext::new(
            uuid::Uuid::new_v4(),
            Arc::new(ServiceRegistry::empty()),
            Arc::new(OnlyBad),
        );
        let agent = super::list_repos::ListReposAgent::new(client);
        let err = agent
            .invoke(&ctx, json!({"account": "all"}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("401"),
            "expected the 401 to surface, got {msg}"
        );
        assert!(
            !msg.contains(TOKEN_BAD),
            "AGENTS.md §6: the token must not appear in the error; got {msg}"
        );
    }

    #[tokio::test]
    async fn me_all_returns_one_row_per_account_with_scopes() {
        let fx = MultiFixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = multi_ctx();
        let agent = super::me::MeAgent::new(client);
        let out = agent.invoke(&ctx, json!({"account": "all"})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let accounts = parsed["data"]["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2, "bad account 401s into account_errors");
        let names: Vec<&str> = accounts
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"default") && names.contains(&"client-a"));
        let client_a = accounts.iter().find(|a| a["name"] == "client-a").unwrap();
        assert_eq!(client_a["user"], "alice");
        assert_eq!(client_a["scopes"], "repo");
        assert!(parsed["data"]["account_errors"].get("client-bad").is_some());
        assert!(!out.contains(TOKEN_A) && !out.contains(TOKEN_BAD) && !out.contains(TEST_TOKEN));
    }

    #[tokio::test]
    async fn repo_account_resolution_tries_accounts_and_caches() {
        let fx = MultiFixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = multi_ctx();
        let agent = super::get_repo::GetRepoAgent::new(client);
        // `org-x/secret` is only visible to client-a: the default
        // account 404s once, then client-a wins and is cached.
        let out = agent
            .invoke(&ctx, json!({"repo": "org-x/secret"}))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["account"], "client-a");
        assert_eq!(parsed["data"]["full_name"], "org-x/secret");
        // Second call must hit the resolution cache: no new
        // /repos/org-x/secret probe from the default account.
        let before = fx
            .requests()
            .iter()
            .filter(|(p, _)| p.starts_with("GET /repos/org-x/secret"))
            .count();
        let out2 = agent
            .invoke(&ctx, json!({"repo": "org-x/secret"}))
            .await
            .unwrap();
        let after = fx
            .requests()
            .iter()
            .filter(|(p, _)| p.starts_with("GET /repos/org-x/secret"))
            .count();
        // default 404 + client-a 200 (resolution) + client-a 200 (the
        // tool's own fetch) = 3; the second invoke is fully cached.
        assert_eq!(before, 3, "default 404 + client-a resolution + tool fetch");
        assert_eq!(
            after, 3,
            "resolution + response cached; second call adds nothing"
        );
        let parsed2: Value = serde_json::from_str(&out2).unwrap();
        assert_eq!(parsed2["data"]["account"], "client-a");
        assert!(!out.contains(TOKEN_A) && !out.contains(TEST_TOKEN));
    }

    #[tokio::test]
    async fn repo_not_readable_by_any_account_lists_names_not_tokens() {
        let fx = MultiFixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = multi_ctx();
        let agent = super::get_repo::GetRepoAgent::new(client);
        let err = agent
            .invoke(&ctx, json!({"repo": "nope/missing"}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("404"), "got {msg}");
        assert!(
            msg.contains("client-a") && msg.contains("default"),
            "names listed: {msg}"
        );
        assert!(
            !msg.contains(TOKEN_A) && !msg.contains(TOKEN_BAD) && !msg.contains(TEST_TOKEN),
            "AGENTS.md §6: no token in the error; got {msg}"
        );
    }

    #[tokio::test]
    async fn single_account_call_uses_that_accounts_token() {
        let fx = MultiFixture::start().await;
        let client = GithubClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = multi_ctx();
        let agent = super::me::MeAgent::new(client);
        let out = agent
            .invoke(&ctx, json!({"account": "client-a"}))
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["data"]["account"], "client-a");
        assert_eq!(parsed["data"]["login"], "alice");
        let reqs = fx.requests();
        let last = reqs.last().unwrap();
        assert_eq!(last.1, format!("Bearer {TOKEN_A}"));
    }
}
