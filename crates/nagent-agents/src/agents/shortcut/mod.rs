//! Shortcut agent family (plan 1795597719369) — ten **read-only**
//! chat tools over the Shortcut REST API v3
//! (`https://api.app.shortcut.com/api/v3`) with the calling user's
//! API token:
//!
//! | Tool | Endpoint |
//! | --- | --- |
//! | `shortcut_me` | `GET /members` (member picked by mention name) |
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
pub mod create_story;
pub mod get_epic;
pub mod get_story;
pub mod list_backlog;
pub mod list_epics;
pub mod list_iterations;
pub mod list_labels;
pub mod list_projects;
pub mod list_teams;
pub mod list_templates;
pub mod list_workflows;
pub mod me;
pub mod search_stories;

pub use client::ShortcutClient;

// The wire client and the JSON projections live in the
// `nagent-shortcut` crate so `nagent-server`'s pure-API dashboard
// endpoints share them without depending on the agent machinery.
// Re-exported here so the tool implementations keep their
// `super::…` import paths (`find_member` stays `pub` because the
// server-side setup probe used it before the extraction; it now
// calls `nagent_shortcut::find_member` directly).
pub use nagent_shortcut::find_member;
pub(crate) use nagent_shortcut::{
    find_team, label_names, mention_name_of, name_of_member, project_story, project_team,
    rank_open_epics,
};

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

/// Confirmation reason for write tools (`shortcut_create_story`).
/// Names the story being created so the approval card is specific.
pub(crate) fn confirm_reason_write(tool: &str, summary: &str) -> ConfirmationDecision {
    ConfirmationDecision::NeedsConfirmation {
        reason: format!(
            "`{tool}` WRITES to the user's Shortcut (app.shortcut.com) workspace through the \
             Shortcut API with their API token: {summary}. The user must explicitly confirm \
             in chat before this call runs. Wait for the user to click \"Autoriser\" (or \
             \"Toujours pour cette session\") in the chat UI; the tool will execute on their \
             next message and you will see its result then.",
        ),
    }
}

/// `UserContext::secret` maps a missing vault row to
/// `Err(CredentialsMissing)`; optional fields treat that as
/// "absent" rather than a hard error.
pub(crate) fn optional_secret(
    res: Result<Option<secrecy::SecretString>, AgentError>,
) -> Result<Option<secrecy::SecretString>, AgentError> {
    match res {
        Err(AgentError::CredentialsMissing { .. }) => Ok(None),
        other => other,
    }
}

/// Load + parse the per-user `story_templates` vault field (a JSON
/// array of `{name, story_type?, labels?, description_template?}`
/// objects). Empty / absent field yields an empty list; malformed
/// JSON is an `InvalidArguments` error (the raw value is never
/// included in the message).
pub(crate) async fn load_templates(ctx: &UserContext) -> Result<Vec<Value>, AgentError> {
    use secrecy::ExposeSecret;
    let Some(secret) = optional_secret(ctx.secret("shortcut", "story_templates").await)? else {
        return Ok(Vec::new());
    };
    let text = secret.expose_secret();
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let parsed: Value = serde_json::from_str(text).map_err(|e| {
        AgentError::InvalidArguments(format!(
            "the saved `story_templates` field is not valid JSON ({e}); fix it in \
             Settings → Integrations → Shortcut"
        ))
    })?;
    match parsed.as_array() {
        Some(arr) => Ok(arr.clone()),
        None => Err(AgentError::InvalidArguments(
            "the saved `story_templates` field must be a JSON array of template objects".into(),
        )),
    }
}

/// Look up one template by name (case-insensitive) in the parsed
/// `story_templates` array.
pub(crate) fn find_template<'a>(templates: &'a [Value], name: &str) -> Option<&'a Value> {
    let q = name.trim().to_lowercase();
    templates.iter().find(|t| {
        t.get("name")
            .and_then(|v| v.as_str())
            .is_some_and(|n| n.to_lowercase() == q)
    })
}

/// Cached GET helper for the intermediate lookups inside
/// `shortcut_create_story` (teams / members / epics). Uses a fixed
/// empty-args cache key under `tool` so repeated creations in one
/// session share the upstream response within `cache_ttl_secs`.
pub(crate) async fn cached_get(
    client: &ShortcutClient,
    ctx: &UserContext,
    tool: &str,
    path: &str,
) -> Result<Value, AgentError> {
    let args = serde_json::json!({});
    if let Some(cached) = client.cache_get(ctx.user_id(), tool, &args) {
        return serde_json::from_str(&cached)
            .map_err(|e| AgentError::AgentFailed(format!("shortcut cache decode: {e}")));
    }
    let raw = client.get(ctx, path, &[]).await?;
    let encoded = serde_json::to_string(&raw)
        .map_err(|e| AgentError::AgentFailed(format!("shortcut encode: {e}")))?;
    client.cache_put(ctx.user_id(), tool, &args, encoded);
    Ok(raw)
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

// `find_member`, `mention_name_of`, `name_of_member`,
// `label_names` and `project_story` are re-exported from
// `nagent_shortcut` at the top of this module — see the block
// right after `pub use client::ShortcutClient;`.

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

    // `mention_name_of` / `project_story` projection tests moved
    // to the `nagent-shortcut` crate together with the functions.
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

    /// Static in-memory `SecretSource` returning the test token
    /// plus the optional vault fields the write path reads
    /// (`username`, `default_team`, `story_templates`).
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
            } else if service == "shortcut" && field == "username" {
                Ok(Some(secrecy::SecretString::from("jane")))
            } else if service == "shortcut" && field == "default_team" {
                Ok(Some(secrecy::SecretString::from(
                    r#"{"id":"team-uuid-1","name":"Engineering"}"#,
                )))
            } else if service == "shortcut" && field == "story_templates" {
                Ok(Some(secrecy::SecretString::from(concat!(
                    r###"[{"name":"bug","story_type":"bug","labels":["bug"],"###,
                    r###""description_template":"## Steps: {details} (story: {name})"}]"###
                ))))
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
        /// Captured request bodies (POST), in request order.
        bodies: Arc<Mutex<Vec<String>>>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let requests: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
            let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let req_clone = requests.clone();
            let bodies_clone = bodies.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut sock, _)) = listener.accept().await else {
                        break;
                    };
                    let req_clone = req_clone.clone();
                    let bodies_clone = bodies_clone.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 4096];
                        // Read headers, then exactly `content-length`
                        // body bytes so POST payloads are captured too.
                        let header_end = loop {
                            match sock.read(&mut chunk).await {
                                Ok(0) => break None,
                                Ok(n) => {
                                    buf.extend_from_slice(&chunk[..n]);
                                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                                    {
                                        break Some(pos + 4);
                                    }
                                }
                                Err(_) => break None,
                            }
                        };
                        let Some(header_end) = header_end else {
                            return;
                        };
                        let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
                        let content_length = head
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        while buf.len() < header_end + content_length {
                            match sock.read(&mut chunk).await {
                                Ok(0) => break,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
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
                        let method = request_line
                            .split_whitespace()
                            .next()
                            .unwrap_or("")
                            .to_string();
                        let path = request_line
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/")
                            .to_string();
                        let body_text = if content_length > 0 {
                            text[header_end..].to_string()
                        } else {
                            String::new()
                        };
                        if method == "POST" {
                            bodies_clone.lock().unwrap().push(body_text.clone());
                        }
                        let (status, reason, extra, body) = route(&method, &path, &body_text);
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
                bodies,
                _task: task,
            }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn requests(&self) -> Vec<(String, String)> {
            self.requests.lock().unwrap().clone()
        }

        fn bodies(&self) -> Vec<String> {
            self.bodies.lock().unwrap().clone()
        }
    }

    /// Route table for the fixture. `method` + `target` (path +
    /// query) are matched exactly; `body` is the raw POST payload.
    fn route(
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, &'static str, Vec<(&'static str, String)>, String) {
        let json = |v: Value| v.to_string();
        if method == "POST" && path == "/stories" {
            // Echo the create payload back as the created story so
            // tests can assert exactly what was sent upstream.
            let sent: Value = serde_json::from_str(body).unwrap_or(json!({}));
            return (
                201,
                "Created",
                vec![],
                json(json!({
                    "id": 555,
                    "name": sent.get("name"),
                    "story_type": sent.get("story_type"),
                    "description": sent.get("description"),
                    "group_id": sent.get("group_id"),
                    "owner_ids": sent.get("owner_ids"),
                    "epic_id": sent.get("epic_id"),
                    "project_id": sent.get("project_id"),
                    "labels": sent.get("labels"),
                    "workflow_state_id": 500000001,
                    "app_url": "https://app.shortcut.com/ws/story/555"
                })),
            );
        }
        if method == "GET" && path == "/teams" {
            return (
                200,
                "OK",
                vec![],
                json(json!([
                    {"id": "team-uuid-1", "name": "Engineering", "mention_name": "eng",
                     "workflow_ids": [500000001], "archived": false,
                     "app_url": "https://app.shortcut.com/ws/eng"},
                    {"id": "team-uuid-2", "name": "Design", "mention_name": "design",
                     "workflow_ids": [500000002], "archived": false}
                ])),
            );
        }
        if method == "GET" && path == "/epics" {
            return (
                200,
                "OK",
                vec![],
                json(json!([
                    {"id": 7, "name": "Mobile app", "state": "started", "archived": false,
                     "description": "iOS and Android apps, offline sync, push notifications, mobile crash reports",
                     "labels": [{"name": "mobile"}]},
                    {"id": 8, "name": "Auth", "state": "started", "archived": false,
                     "description": "Login, SSO, OAuth, session handling, password reset"},
                    {"id": 9, "name": "Legacy migration", "state": "done", "archived": false,
                     "description": "old auth mobile cleanup"}
                ])),
            );
        }
        if path == "/members" {
            return (
                200,
                "OK",
                vec![],
                json(json!([
                    {
                        "id": "12345678-9012-3456-7890-123456789012",
                        "profile": {"name": "Jane Doe", "mention_name": "jane", "email_address": "jane@example.com"},
                        "disabled": false
                    },
                    {
                        "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                        "profile": {"name": "John Roe", "mention_name": "john", "email_address": "john@example.com"},
                        "disabled": false
                    }
                ])),
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

    #[test]
    fn find_member_matches_mention_name_case_and_at_insensitive() {
        let parsed = json!([
            {"id": "uuid-1", "profile": {"name": "Jane Doe", "mention_name": "jane", "email_address": "jane@example.com"}},
            {"id": "uuid-2", "profile": {"name": "John Roe", "mention_name": "john", "email_address": "john@example.com"}}
        ]);
        let m = find_member(&parsed, Some("@JANE")).expect("match on mention_name");
        assert_eq!(m["id"], "uuid-1");
        let m = find_member(&parsed, Some("john@example.com")).expect("match on email");
        assert_eq!(m["id"], "uuid-2");
        let m = find_member(&parsed, Some("John Roe")).expect("match on display name");
        assert_eq!(m["id"], "uuid-2");
        assert!(find_member(&parsed, Some("nobody")).is_none());
        assert!(
            find_member(&parsed, None).is_none(),
            "ambiguous without handle"
        );
    }

    #[test]
    fn find_member_single_member_list_needs_no_handle() {
        let parsed = json!({
            "id": "uuid-1",
            "profile": {"name": "Jane Doe", "mention_name": "jane"}
        });
        let m = find_member(&parsed, None).expect("single object works without handle");
        assert_eq!(m["profile"]["mention_name"], "jane");
        let arr = json!([{"id": "uuid-1", "profile": {"mention_name": "jane"}}]);
        let m = find_member(&arr, None).expect("single-element array works");
        assert_eq!(m["id"], "uuid-1");
    }

    #[tokio::test]
    async fn happy_path_get_members_sends_shortcut_token_and_parses() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let v = client
            .get(&ctx, "/members", &[])
            .await
            .expect("GET /members");
        let m = find_member(&v, Some("jane")).expect("jane is in the fixture list");
        assert_eq!(m["profile"]["mention_name"], "jane");
        let reqs = fx.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, "GET /members HTTP/1.1");
        assert_eq!(reqs[0].1, TEST_TOKEN);
    }

    #[tokio::test]
    async fn me_agent_resolves_member_from_list_by_stored_username() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let agent = me::MeAgent::new(client);
        let ctx = test_ctx();
        let out = agent.invoke(&ctx, json!({})).await.expect("invoke");
        let v: Value = serde_json::from_str(&out).expect("json");
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["mention_name"], "jane");
        assert_eq!(v["data"]["id"], "12345678-9012-3456-7890-123456789012");
        assert_eq!(v["data"]["email"], "jane@example.com");
        let reqs = fx.requests();
        assert!(
            reqs.iter()
                .any(|(line, _)| line.starts_with("GET /members ")),
            "shortcut_me must call GET /members, not /members/me; got {reqs:?}"
        );
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

    #[tokio::test]
    async fn list_teams_agent_returns_projected_teams() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let agent = super::list_teams::ListTeamsAgent::new(client);
        let out = agent.invoke(&ctx, json!({})).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["count"], 2);
        assert_eq!(v["data"]["teams"][0]["id"], "team-uuid-1");
        assert_eq!(v["data"]["teams"][0]["mention_name"], "eng");
        assert_eq!(fx.requests()[0].0, "GET /teams HTTP/1.1");
    }

    #[tokio::test]
    async fn create_story_applies_template_default_team_owner_and_auto_epic() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let agent = super::create_story::CreateStoryAgent::new(client);
        let out = agent
            .invoke(
                &ctx,
                json!({
                    "name": "Crash at login on the mobile app",
                    "template": "bug",
                    "details": "open the app then log in",
                }),
            )
            .await
            .expect("invoke");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["story"]["id"], 555);
        assert_eq!(v["data"]["template"], "bug");
        assert_eq!(v["data"]["team"]["id"], "team-uuid-1");
        assert_eq!(v["data"]["team"]["source"], "default_team");
        assert_eq!(
            v["data"]["owner_id"],
            "12345678-9012-3456-7890-123456789012"
        );
        let m = &v["data"]["epic_match"];
        assert_eq!(m["auto"], true);
        assert_eq!(m["epic_id"], 7, "BM25 must pick the Mobile app epic");

        // Exactly one POST reached upstream, carrying the token,
        // and its body has the resolved group/owner/epic/labels.
        let reqs = fx.requests();
        let posts: Vec<_> = reqs
            .iter()
            .filter(|(l, _)| l.starts_with("POST /stories"))
            .collect();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].1, TEST_TOKEN, "POST must carry the token header");
        let bodies = fx.bodies();
        assert_eq!(bodies.len(), 1);
        let sent: Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(sent["name"], "Crash at login on the mobile app");
        assert_eq!(sent["story_type"], "bug", "type comes from the template");
        assert_eq!(sent["group_id"], "team-uuid-1", "default_team → group_id");
        assert_eq!(
            sent["owner_ids"][0], "12345678-9012-3456-7890-123456789012",
            "owner = caller's member resolved from saved username"
        );
        assert_eq!(sent["epic_id"], 7);
        assert_eq!(sent["labels"][0]["name"], "bug");
        let desc = sent["description"].as_str().unwrap();
        assert!(
            desc.contains("open the app then log in"),
            "{{details}} substituted: {desc}"
        );
        assert!(
            desc.contains("Crash at login on the mobile app"),
            "{{name}} substituted: {desc}"
        );
    }

    #[tokio::test]
    async fn create_story_with_unclear_type_asks_the_user_and_posts_nothing() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let agent = super::create_story::CreateStoryAgent::new(client);
        let err = agent
            .invoke(&ctx, json!({"name": "Something vague"}))
            .await
            .unwrap_err();
        match err {
            AgentError::InvalidArguments(m) => {
                assert!(m.contains("feature, a bug or a chore"), "got: {m}");
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
        assert!(
            fx.bodies().is_empty(),
            "no POST may reach upstream when the type is unclear"
        );
    }

    #[tokio::test]
    async fn create_story_unknown_template_lists_available_names() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let agent = super::create_story::CreateStoryAgent::new(client);
        let err = agent
            .invoke(
                &ctx,
                json!({"name": "X", "template": "nope", "story_type": "feature"}),
            )
            .await
            .unwrap_err();
        match err {
            AgentError::InvalidArguments(m) => assert!(m.contains("bug"), "got: {m}"),
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
        assert!(fx.bodies().is_empty());
    }

    #[tokio::test]
    async fn create_story_explicit_team_arg_wins_over_default_team() {
        let fx = Fixture::start().await;
        let client = ShortcutClient::new(test_cfg(fx.base_url()), reqwest::Client::new());
        let ctx = test_ctx();
        let agent = super::create_story::CreateStoryAgent::new(client);
        let out = agent
            .invoke(
                &ctx,
                json!({
                    "name": "New onboarding flow",
                    "story_type": "feature",
                    "team": "design",          // mention name of team-uuid-2
                    "auto_epic": false,
                }),
            )
            .await
            .expect("invoke");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["data"]["team"]["id"], "team-uuid-2");
        assert_eq!(v["data"]["team"]["source"], "arg");
        assert!(v["data"].get("epic_match").is_none());
        let sent: Value = serde_json::from_str(&fx.bodies()[0]).unwrap();
        assert_eq!(sent["group_id"], "team-uuid-2");
        assert!(sent.get("epic_id").is_none());
    }
}
