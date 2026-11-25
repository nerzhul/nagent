//! Shortcut agent family (plan 1795597719369) — ten **read-only**
//! chat tools over the Shortcut REST API v3
//! (`https://api.app.shortcut.com/api/v3`) with the calling user's
//! API token:
//!
//! | Tool | Endpoint |
//! | --- | --- |
//! | `shortcut_me` | `GET /members/me` |
//! | `shortcut_list_workflows` | `GET /workflows` |
//! | `shortcut_list_projects` | `GET /projects` |
//! | `shortcut_list_epics` | `GET /epics` |
//! | `shortcut_get_epic` | `GET /epics/{id}` |
//! | `shortcut_list_iterations` | `GET /iterations` |
//! | `shortcut_search_stories` | `GET /search/stories` |
//! | `shortcut_list_backlog` | `GET /search/stories` with the configured backlog query |
//! | `shortcut_get_story` | `GET /stories/{id}` (+ `/comments`) |
//! | `shortcut_list_labels` | `GET /labels` |
//!
//! All tools share one [`client::ShortcutClient`] shape (per-tool
//! instance built by the descriptor table, all draining the
//! shared `EgressPool::public()` client) and one short-lived
//! in-process cache keyed by `(user_id, tool, args)` — the
//! Shortcut quota is 200 req/min per token, so the
//! `cache_ttl_secs` default of 60 s keeps repeated questions
//! cheap.
//!
//! Write tools (create story / comment / move story) are
//! deliberately out of scope for v1; the descriptor table does
//! not register them, so the LLM tool loop returns "unknown
//! tool" if a model tries.
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
pub mod get_epic;
pub mod get_story;
pub mod list_backlog;
pub mod list_epics;
pub mod list_iterations;
pub mod list_labels;
pub mod list_projects;
pub mod list_workflows;
pub mod me;
pub mod search_stories;

pub use client::ShortcutClient;

use serde_json::Value;

use crate::agents::{AgentError, ConfirmationDecision, UserContext};
use crate::ShortcutAgentConfig;

/// Run one tool end-to-end: cache lookup → authenticated GET →
/// projection → JSON encode → cache store. `project` receives
/// the raw upstream JSON and the agent config and returns the
/// LLM-facing envelope.
pub(crate) async fn run<F>(
    client: &ShortcutClient,
    ctx: &UserContext,
    tool: &str,
    args: &Value,
    path: &str,
    query: Vec<(String, String)>,
    project: F,
) -> Result<String, AgentError>
where
    F: FnOnce(&Value, &ShortcutAgentConfig) -> Value,
{
    if let Some(cached) = client.cache_get(ctx.user_id(), tool, args) {
        tracing::debug!(agent = tool, user_id = %ctx.user_id(), "shortcut cache hit");
        return Ok(cached);
    }
    let raw = client.get(ctx, path, &query).await?;
    let out = project(&raw, &client.cfg);
    let encoded = serde_json::to_string(&out)
        .map_err(|e| AgentError::AgentFailed(format!("shortcut encode: {e}")))?;
    client.cache_put(ctx.user_id(), tool, args, encoded.clone());
    Ok(encoded)
}

/// Shared confirmation reason for every `shortcut_*` tool. The
/// reason names the tool so the approval card is specific.
pub(crate) fn confirm_reason(tool: &str) -> ConfirmationDecision {
    ConfirmationDecision::NeedsConfirmation {
        reason: format!(
            "`{tool}` reads the user's Shortcut (app.shortcut.com) workspace through the \
             Shortcut API with their API token. The user must explicitly confirm in chat \
             before this call runs. Wait for the user to click \"Autoriser\" in the chat \
             UI; the tool will execute on their next message and you will see its result \
             then.",
        ),
    }
}

/// Parse the `limit` argument: default `default`, clamped to
/// `[1, cfg.max_items]`.
pub(crate) fn parse_limit(args: &Value, cfg: &ShortcutAgentConfig, default: usize) -> usize {
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

/// Parse a required positive-integer id argument (`story_id`,
/// `epic_id`). Shortcut entity ids are positive integers.
pub(crate) fn parse_id(args: &Value, key: &str) -> Result<u64, AgentError> {
    args.get(key)
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            AgentError::InvalidArguments(format!(
                "`{key}` is required and must be a positive integer (the numeric Shortcut id)"
            ))
        })
}

/// Parse a required non-empty string argument.
pub(crate) fn parse_required_str(args: &Value, key: &str) -> Result<String, AgentError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            AgentError::InvalidArguments(format!(
                "`{key}` is required and must be a non-empty string"
            ))
        })
}

/// Truncate a JSON string field (description, comment text, …)
/// to `cap` bytes at a char boundary, appending a marker.
/// Returns the original value when it fits.
pub(crate) fn cap_text(value: &Value, cap: usize) -> Value {
    match value {
        Value::String(s) => Value::String(client::truncate(s, cap)),
        other => other.clone(),
    }
}

/// Extract `mention_name` from a Shortcut member object. The v3
/// member nests it under `profile`; a top-level field is accepted
/// as a fallback so the projection survives minor API drift.
pub(crate) fn mention_name_of(v: &Value) -> Value {
    v.get("profile")
        .and_then(|p| p.get("mention_name"))
        .or_else(|| v.get("mention_name"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Extract `name` from a Shortcut member object (same nesting
/// as [`mention_name_of`]).
pub(crate) fn name_of_member(v: &Value) -> Value {
    v.get("profile")
        .and_then(|p| p.get("name"))
        .or_else(|| v.get("name"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Extract the label names from a Shortcut `labels` array.
pub(crate) fn label_names(v: &Value) -> Vec<Value> {
    v.get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| arr.iter().filter_map(|l| l.get("name").cloned()).collect())
        .unwrap_or_default()
}

/// Project one Shortcut story object into the LLM-facing row.
/// Used by the list / search tools; `shortcut_get_story` adds the
/// capped description and comments.
pub(crate) fn project_story(s: &Value) -> Value {
    serde_json::json!({
        "id": s.get("id"),
        "name": s.get("name"),
        "story_type": s.get("story_type"),
        "workflow_state_id": s.get("workflow_state_id"),
        "project_id": s.get("project_id"),
        "epic_id": s.get("epic_id"),
        "iteration_id": s.get("iteration_id"),
        "estimate": s.get("estimate"),
        "labels": label_names(s),
        "owner_ids": s.get("owner_ids"),
        "requested_by_id": s.get("requested_by_id"),
        "deadline": s.get("deadline"),
        "archived": s.get("archived"),
        "created_at": s.get("created_at"),
        "updated_at": s.get("updated_at"),
        "app_url": s.get("app_url"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> ShortcutAgentConfig {
        ShortcutAgentConfig::default()
    }

    #[test]
    fn parse_limit_defaults_and_caps() {
        assert_eq!(parse_limit(&json!({}), &cfg(), 50), 50);
        assert_eq!(parse_limit(&json!({"limit": 5}), &cfg(), 50), 5);
        assert_eq!(parse_limit(&json!({"limit": 99999}), &cfg(), 50), 200);
        assert_eq!(parse_limit(&json!({"limit": 0}), &cfg(), 50), 1);
    }

    #[test]
    fn parse_enum_validates_against_allowlist() {
        assert_eq!(
            parse_enum(
                &json!({}),
                "state",
                &["unstarted", "started", "done"],
                "all"
            )
            .unwrap(),
            "all"
        );
        assert_eq!(
            parse_enum(
                &json!({"state": "done"}),
                "state",
                &["unstarted", "started", "done"],
                "all"
            )
            .unwrap(),
            "done"
        );
        assert!(parse_enum(
            &json!({"state": "merged"}),
            "state",
            &["unstarted", "started", "done"],
            "all"
        )
        .is_err());
    }

    #[test]
    fn parse_id_requires_positive_integer() {
        assert_eq!(parse_id(&json!({"story_id": 42}), "story_id").unwrap(), 42);
        assert!(parse_id(&json!({"story_id": 0}), "story_id").is_err());
        assert!(parse_id(&json!({}), "story_id").is_err());
        assert!(parse_id(&json!({"story_id": "42"}), "story_id").is_err());
    }

    #[test]
    fn parse_required_str_trims_and_rejects_empty() {
        assert_eq!(
            parse_required_str(&json!({"query": "  is:story  "}), "query").unwrap(),
            "is:story"
        );
        assert!(parse_required_str(&json!({"query": "   "}), "query").is_err());
        assert!(parse_required_str(&json!({}), "query").is_err());
    }

    #[test]
    fn cap_text_truncates_long_strings() {
        let long = "x".repeat(1000);
        let capped = cap_text(&json!(long), 100);
        assert!(capped.as_str().unwrap().ends_with("…[truncated]"));
        assert_eq!(cap_text(&json!("short"), 100), json!("short"));
        assert_eq!(cap_text(&json!(42), 100), json!(42));
    }

    #[test]
    fn mention_name_reads_profile_with_fallback() {
        assert_eq!(
            mention_name_of(&json!({"profile": {"mention_name": "jane"}})),
            json!("jane")
        );
        assert_eq!(
            mention_name_of(&json!({"mention_name": "jane"})),
            json!("jane")
        );
        assert_eq!(mention_name_of(&json!({})), Value::Null);
    }

    #[test]
    fn project_story_picks_llm_fields() {
        let raw = json!({
            "id": 123,
            "name": "Fix login",
            "story_type": "bug",
            "workflow_state_id": 500000001,
            "epic_id": 7,
            "iteration_id": null,
            "estimate": 3,
            "labels": [{"name": "backend"}],
            "description": "long body — dropped from the list projection",
            "app_url": "https://app.shortcut.com/ws/story/123"
        });
        let p = project_story(&raw);
        assert_eq!(p["id"], 123);
        assert_eq!(p["story_type"], "bug");
        assert_eq!(p["labels"], json!(["backend"]));
        assert!(p.get("description").is_none());
    }
}

/// Loopback HTTP fixture + wire-level tests for the Shortcut
/// client: happy path, 404, 401, 429, body cap, search-query
/// passthrough, backlog default query, cache, and the AGENTS.md
/// §6 redaction rule (the token never appears in an error path).
#[cfg(test)]
mod loopback {
    use super::*;
    use crate::agents::{Agent, SecretSource, UserContext};
    use crate::ServiceRegistry;
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TEST_TOKEN: &str = "sc_test_token_0123456789abcdef";

    /// Static in-memory `SecretSource` returning the test token.
    struct StaticSecrets;

    #[async_trait::async_trait]
    impl SecretSource for StaticSecrets {
        async fn fetch(
            &self,
            _user_id: uuid::Uuid,
            service: &str,
            field: &str,
        ) -> Result<Option<secrecy::SecretString>, AgentError> {
            if service == "shortcut" && field == "api_token" {
                Ok(Some(secrecy::SecretString::from(TEST_TOKEN)))
            } else {
                Ok(None)
            }
        }
    }

    struct Fixture {
        addr: SocketAddr,
        /// Captured request lines (`GET /path?query HTTP/1.1`)
        /// plus the `Shortcut-Token` header value of each request.
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
                        let token = text
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("shortcut-token:"))
                            .map(|l| {
                                l.split_once(':')
                                    .map(|(_, v)| v.trim().to_string())
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default();
                        req_clone
                            .lock()
                            .unwrap()
                            .push((request_line.clone(), token));
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
        if path == "/members/me" {
            return (
                200,
                "OK",
                vec![],
                json(json!({
                    "id": "12345678-9012-3456-7890-123456789012",
                    "profile": {"name": "Jane Doe", "mention_name": "jane", "email_address": "jane@example.com"},
                    "disabled": false
                })),
            );
        }
        if path.starts_with("/search/stories") {
            return (
                200,
                "OK",
                vec![],
                json(json!({
                    "data": [{
                        "id": 101,
                        "name": "Add dark mode",
                        "story_type": "feature",
                        "workflow_state_id": 500000001,
                        "iteration_id": null,
                        "epic_id": 7,
                        "estimate": 5,
                        "labels": [{"name": "ui"}],
                        "app_url": "https://app.shortcut.com/ws/story/101"
                    }],
                    "total": 1,
                    "next": null
                })),
            );
        }
        if path.starts_with("/stories/101/comments") {
            return (
                200,
                "OK",
                vec![],
                json(
                    json!([{"id": "c1", "author_id": "u1", "text": "looks good", "created_at": "2026-11-20T10:00:00Z"}]),
                ),
            );
        }
        if path.starts_with("/stories/101") {
            return (
                200,
                "OK",
                vec![],
                json(json!({
                    "id": 101,
                    "name": "Add dark mode",
                    "story_type": "feature",
                    "workflow_state_id": 500000001,
                    "description": "x".repeat(500),
                    "app_url": "https://app.shortcut.com/ws/story/101"
                })),
            );
        }
        if path == "/projects" {
            return (
                200,
                "OK",
                vec![],
                json(json!([{
                    "id": 9,
                    "name": "Website",
                    "state": "started",
                    "archived": false,
                    "app_url": "https://app.shortcut.com/ws/projects/9"
                }])),
            );
        }
        if path == "/boom401" {
            return (
                401,
                "Unauthorized",
                vec![],
                json(json!({"message": "Invalid API token"})),
            );
        }
        if path == "/ratelimited" {
            return (
                429,
                "Too Many Requests",
                vec![("retry-after", "30".to_string())],
                json(json!({"message": "Rate limit exceeded"})),
            );
        }
        (
            404,
            "Not Found",
            vec![],
            json(json!({"message": "Not Found"})),
        )
    }

    fn test_cfg(base_url: String) -> ShortcutAgentConfig {
        ShortcutAgentConfig {
            timeout_ms: 5_000,
            max_items: 200,
            max_body_bytes: 64,
            cache_ttl_secs: 60,
            allowlist: vec!["127.0.0.1".to_string()],
            base_url,
            backlog_query: "is:story is:backlog !is:archived".to_string(),
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
    async fn happy_path_get_members_me_sends_shortcut_token_and_parses() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let v = client
            .get(&ctx, "/members/me", &[])
            .await
            .expect("GET /members/me");
        assert_eq!(v["profile"]["mention_name"], "jane");
        let reqs = fx.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, "GET /members/me HTTP/1.1");
        assert_eq!(reqs[0].1, TEST_TOKEN);
    }

    #[tokio::test]
    async fn not_found_maps_to_upstream_404() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let err = client.get(&ctx, "/stories/999999", &[]).await.unwrap_err();
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
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let err = client.get(&ctx, "/boom401", &[]).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("401"), "expected a 401 hint, got {msg}");
        assert!(
            !msg.contains(TEST_TOKEN),
            "AGENTS.md §6: the token must never appear in an error string; got {msg}"
        );
        // The fixture DID receive the token in the header (the
        // auth was actually attempted) — the redaction is about
        // the error path, not about not sending it.
        assert_eq!(fx.requests()[0].1, TEST_TOKEN);
    }

    #[tokio::test]
    async fn too_many_requests_maps_to_rate_limited_with_hint() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
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
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let args = json!({"limit": 10});
        let first = run(
            &client,
            &ctx,
            "shortcut_list_projects",
            &args,
            "/projects",
            vec![],
            |raw, _| raw.clone(),
        )
        .await
        .unwrap();
        let second = run(
            &client,
            &ctx,
            "shortcut_list_projects",
            &args,
            "/projects",
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
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let args = json!({"query": "is:story is:unstarted owner:jane", "limit": 5});
        let out = run(
            &client,
            &ctx,
            "shortcut_search_stories",
            &args,
            "/search/stories",
            vec![
                (
                    "query".to_string(),
                    "is:story is:unstarted owner:jane".to_string(),
                ),
                ("page_size".to_string(), "5".to_string()),
            ],
            |raw, _| json!({"total": raw.get("total"), "stories": raw.get("data")}),
        )
        .await
        .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["total"], 1);
        let req_line = &fx.requests()[0].0;
        assert!(
            req_line.contains("query=is%3Astory+is%3Aunstarted+owner%3Ajane"),
            "query must be percent-encoded on the wire; got {req_line}"
        );
        assert!(req_line.contains("page_size=5"));
    }

    #[tokio::test]
    async fn list_backlog_uses_the_configured_default_query() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let agent = super::list_backlog::ListBacklogAgent::new(client);
        let out = agent.invoke(&ctx, json!({})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["query"], "is:story is:backlog !is:archived");
        assert_eq!(parsed["data"]["count"], 1);
        assert_eq!(parsed["data"]["stories"][0]["id"], 101);
        let req_line = &fx.requests()[0].0;
        assert!(
            req_line.contains("query=is%3Astory+is%3Abacklog+%21is%3Aarchived"),
            "the configured backlog query must be sent verbatim; got {req_line}"
        );
    }

    #[tokio::test]
    async fn get_story_caps_description_and_includes_comments() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let agent = super::get_story::GetStoryAgent::new(client);
        let out = agent.invoke(&ctx, json!({"story_id": 101})).await.unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["data"]["story"]["id"], 101);
        assert!(parsed["data"]["description"]
            .as_str()
            .unwrap()
            .ends_with("…[truncated]"));
        assert_eq!(parsed["data"]["comments"][0]["text"], "looks good");
        // Two upstream calls: the story + its comments.
        assert_eq!(fx.requests().len(), 2);
    }
}
