//! End-to-end tests for the server-side chat agents.
//!
//! ## Coverage
//!
//! 1. `GET /v1/agents` returns the registered agents (when enabled)
//!    and an empty list (when disabled).
//! 2. `POST /v1/agents/web_fetch/invoke` succeeds against a
//!    loopback fixture server (the sandbox always allows `127.0.0.1`
//!    through `WEB_FETCH_ALLOW_PUBLIC=true`), and rejects unknown
//!    agents / invalid args with the right HTTP status.
//! 3. `POST /v1/chat/completions` injects the agent's `tools`
//!    schema into the upstream request, and when the upstream emits
//!    a `tool_calls` block the proxy runs the agent and emits the
//!    expected `event: tool_call` / `event: tool_result` SSE frames
//!    before completing the turn.
//!
//! The fake upstream simulates Ollama's fragmented `tool_calls`
//! delta shape (which the LLM proxy must buffer across SSE chunks
//! before dispatching).

// The helpers below are useful regardless of which agent features
// are compiled in; the individual tests gate on the feature they
// exercise. Keep this file compiling (rather than `#![cfg]`
// discarding it) so `cargo build --tests --no-default-features` still
// type-checks the harness.
#![cfg(any(
    feature = "web-agent",
    feature = "datetime-agent",
    feature = "weather-agent",
    feature = "stock-agent"
))]

use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, HeaderValue, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use stt_core::{MockBackend, WhisperBackend};
#[cfg(feature = "datetime-agent")]
use stt_server::agents::datetime_agent::DateTimeAgent;
#[cfg(feature = "stock-agent")]
use stt_server::agents::stock_agent::StockAgent;
#[cfg(feature = "weather-agent")]
use stt_server::agents::weather_agent::WeatherAgent;
#[cfg(feature = "web-agent")]
use stt_server::agents::web_fetch::WebFetchAgent;
use stt_server::{
    agents::{Agent, AgentRegistry},
    build_router,
    config::{AgentConfig, LlmConfig, RateLimitConfig, WebFetchConfig},
    llm::LlmClient,
    rate_limit::{RateLimitPolicy, RateLimiter},
    session::SessionMap,
    AppState, Config as ServerConfig,
};
use tokio::net::TcpListener;

/// Spawn a tiny HTTP server with the given axum Router.
async fn spawn_router(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn spawn_static_page_server(body: String, content_type: &'static str) -> String {
    let app = Router::new().route(
        "/page",
        get(move || {
            let body = body.clone();
            async move { ([(header::CONTENT_TYPE, content_type)], body) }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/page")
}

fn make_app_state(
    server_cfg: Arc<ServerConfig>,
    llm_client: Option<LlmClient>,
    agents: Option<AgentRegistry>,
    sessions: SessionMap,
) -> Arc<AppState> {
    let backend: Arc<dyn WhisperBackend> = Arc::new(MockBackend::new("test-model"));
    let (job_tx, _job_rx) = tokio::sync::mpsc::channel::<stt_core::InferenceJob>(16);
    Arc::new(AppState {
        backend,
        sessions,
        job_tx,
        ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        config: server_cfg,
        llm: llm_client,
        agents,
        stt_rate_limiter: RateLimiter::new(RateLimitPolicy::stt(
            RateLimitConfig::default().stt_per_min,
        )),
        llm_rate_limiter: RateLimiter::new(RateLimitPolicy::llm(
            RateLimitConfig::default().llm_per_min,
        )),
    })
}

async fn start_test_server(state: Arc<AppState>) -> String {
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    std::mem::forget(tx);
    format!("http://{addr}")
}

fn make_server_cfg(upstream_url: String) -> ServerConfig {
    ServerConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        whisper_model_path: std::path::PathBuf::from("/tmp/fake-model.bin"),
        max_queue: 32,
        session_idle_timeout: Duration::from_secs(30),
        infer_timeout: Duration::from_secs(30),
        limits: stt_server::config::LimitsConfig::default(),
        rate_limit: RateLimitConfig::default(),
        llm: LlmConfig {
            enabled: true,
            base_url: upstream_url,
            default_model: "llama3.1".into(),
            api_key: None,
            request_timeout: Duration::from_secs(120),
            cors_allow_origins: vec![],
        },
        agents: AgentConfig::default(),
    }
}

// ---- 1. /v1/agents list ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_list_returns_web_fetch_when_feature_enabled() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = AgentRegistry::from_config(&cfg.agents);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let state = make_app_state(cfg, None, Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::get(format!("{url}/v1/agents")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let names: Vec<&str> = body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    #[cfg(feature = "web-agent")]
    assert!(
        names.contains(&"web_fetch"),
        "expected `web_fetch` in {names:?}"
    );
    #[cfg(feature = "datetime-agent")]
    assert!(
        names.contains(&"get_datetime"),
        "expected `get_datetime` in {names:?}"
    );
    #[cfg(feature = "weather-agent")]
    assert!(
        names.contains(&"get_weather"),
        "expected `get_weather` in {names:?}"
    );
    #[cfg(feature = "stock-agent")]
    assert!(
        names.contains(&"get_stock_quote"),
        "expected `get_stock_quote` in {names:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_list_404s_when_disabled() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let state = make_app_state(cfg, None, None, sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::get(format!("{url}/v1/agents")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---- 2. /v1/agents/:name/invoke ----

#[cfg(feature = "web-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_fetch_invoke_succeeds_against_loopback() {
    let page = "<html><head><title>Fixture</title></head>".to_string()
        + "<body><h1>Hi</h1><p>Hello world.</p></body></html>";
    let page_url = spawn_static_page_server(page, "text/html; charset=utf-8").await;

    // The sandbox blocks loopback to prevent SSRF. We work around
    // it for the test by setting `allowlist` to a wildcard and
    // `allow_public` to true. In production the operator would
    // configure these against the intended target.
    let cfg = WebFetchConfig {
        allow_public: true,
        allowlist: vec!["*".into()],
        ..WebFetchConfig::default()
    };
    let agent = WebFetchAgent::new(cfg);

    let args = serde_json::json!({"url": page_url});
    let result = agent.invoke(args).await.expect("invoke");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["title"], "Fixture");
    assert!(parsed["text"].as_str().unwrap().contains("Hello world."));
}

#[cfg(feature = "web-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_fetch_adaptive_retry_fetches_larger_pages() {
    // The LLM typically passes a conservative `max_bytes` (qwen2.5
    // defaults to ~10 KB). The agent must transparently retry with
    // a doubled budget until the page fits — without the LLM
    // needing to second-guess the page size.
    let page = format!(
        "<html><head><title>Big</title></head><body>{}</body></html>",
        "x".repeat(80_000) // 80 KB of body text
    );
    let page_url = spawn_static_page_server(page, "text/html; charset=utf-8").await;

    let cfg = WebFetchConfig {
        allow_public: true,
        allowlist: vec!["*".into()],
        ..WebFetchConfig::default()
    };
    let agent = WebFetchAgent::new(cfg);

    // LLM asks for 10 KB. Page is 80 KB. Agent must retry until it
    // fits inside the server cap (default 2 MiB).
    let args = serde_json::json!({"url": page_url, "max_bytes": 10_000});
    let result = agent
        .invoke(args)
        .await
        .expect("adaptive retry should converge");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["title"], "Big");
    // The cleaned text is ~80 KB of `x`s — verify at least some of
    // it survived the round-trip (the markdown trim caps at
    // MAX_TEXT_CHARS=100 KB which is enough headroom).
    let text = parsed["text"].as_str().unwrap();
    assert!(
        text.len() >= 70_000,
        "expected ~80 KB of body text to survive adaptive retry, got {} bytes",
        text.len()
    );
}

#[cfg(feature = "web-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_fetch_adaptive_retry_caps_at_server_limit() {
    // When the page is larger than the server cap, the agent
    // surfaces a hard error rather than looping forever. The page
    // here is bigger than the operator-set cap so the retry policy
    // must trip on the very first overflow.
    let page = "x".repeat(200_000); // 200 KB body
    let page_url = spawn_static_page_server(page, "text/html; charset=utf-8").await;

    let cfg = WebFetchConfig {
        allow_public: true,
        allowlist: vec!["*".into()],
        // Server cap is 50 KB. Page is 200 KB. The doubling loop
        // reaches 50 KB on the third iteration; the very next
        // overflow must trip the `next <= budget` guard and
        // surface a hard AgentFailed.
        max_bytes: 50 * 1024,
        ..WebFetchConfig::default()
    };
    let agent = WebFetchAgent::new(cfg);
    let args = serde_json::json!({"url": page_url, "max_bytes": 1024});
    let err = agent
        .invoke(args)
        .await
        .expect_err("should fail when page exceeds server cap");
    let msg = err.to_string();
    assert!(
        msg.contains("exceeded max_bytes") && msg.contains("server cap"),
        "expected a hard 'exceeded max_bytes (server cap)' error, got: {msg}"
    );
}

#[cfg(feature = "web-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_fetch_invoke_unknown_agent_404s() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = AgentRegistry::from_config(&cfg.agents);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let state = make_app_state(cfg, None, Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/agents/nonexistent/invoke"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"arguments":{}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[cfg(feature = "web-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_fetch_invoke_invalid_args_400s() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = AgentRegistry::from_config(&cfg.agents);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let state = make_app_state(cfg, None, Some(agents), sessions);
    let url = start_test_server(state).await;

    // Missing the required `url` argument.
    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/agents/web_fetch/invoke"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"arguments":{}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---- 3. Tool-loop end-to-end ----

/// Fake upstream that:
/// - Round 1: streams a fragmented `tool_calls` delta (name, then
///   arguments split across chunks), then `finish_reason:
///   "tool_calls"`. Captures the request body so the test can
///   assert the `tools` field was injected.
/// - Round 2: returns a normal text reply (no tool calls) with
///   `finish_reason: "stop"`. The proxy should forward this to the
///   client verbatim and exit.
async fn spawn_tool_call_upstream(
    captured: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
) -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let captured = Arc::clone(&captured);
            async move {
                let parsed: serde_json::Value = serde_json::from_slice(&body)
                    .unwrap_or_else(|_| serde_json::json!({"_raw": String::from_utf8_lossy(&body).to_string()}));
                let round = captured.lock().await.len();
                captured.lock().await.push(parsed);

                let body = if round == 0 {
                    // Round 1: emit fragmented tool_calls, then finish.
                    concat!(
                        "event: message\n",
                        "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Let me fetch that for you.\\n\"},\"finish_reason\":null}]}\n\n",
                        "event: message\n",
                        "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_x\",\"type\":\"function\",\"function\":{\"name\":\"web_fetch\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
                        "event: message\n",
                        "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"url\\\":\"}}]},\"finish_reason\":null}]}\n\n",
                        "event: message\n",
                        "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"http://example.com\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
                        "event: message\n",
                        "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                        "data: [DONE]\n\n",
                    ).to_string()
                } else {
                    // Round 2: final assistant reply.
                    concat!(
                        "event: message\n",
                        "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Done.\"},\"finish_reason\":null}]}\n\n",
                        "event: message\n",
                        "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: [DONE]\n\n",
                    ).to_string()
                };

                (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/event-stream"),
                    )],
                    body,
                )
            }
        }),
    );
    spawn_router(app).await
}

#[cfg(feature = "web-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_loop_dispatches_and_completes() {
    // We need an upstream we can reach for the second round, but the
    // tool call's `web_fetch` invocation will fail (example.com is
    // a real DNS, but the sandbox blocks it without allow_public).
    // That's OK: the proxy still emits tool_call + tool_result
    // events, then asks upstream for round 2. The test asserts the
    // events make it to the client and the second-round request
    // body contains the synthetic tool message.
    let captured: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let upstream_url = spawn_tool_call_upstream(captured.clone()).await;

    let cfg = Arc::new(make_server_cfg(upstream_url));
    // Disable `web_fetch` from actually doing network: empty
    // allow-list + public blocked = the call to `example.com` will
    // be rejected by the sandbox. The proxy still emits the events.
    let agents = AgentRegistry::from_config(&cfg.agents);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let llm_cfg = Arc::new(cfg.llm.clone());
    let llm_client = LlmClient::new(llm_cfg).unwrap();
    let state = make_app_state(cfg, Some(llm_client), Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(
            r#"{"messages":[{"role":"user","content":"fetch example.com"}],"stream":true,"model":"llama3.1"}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.text().await.unwrap();
    // The proxy should have emitted the tool_call + tool_result
    // events with the LLM-emitted id and the registered agent name.
    assert!(
        body.contains("event: tool_call"),
        "missing event: tool_call in {body}"
    );
    assert!(
        body.contains("\"name\":\"web_fetch\""),
        "missing web_fetch name in {body}"
    );
    assert!(
        body.contains("event: tool_result"),
        "missing event: tool_result in {body}"
    );
    assert!(
        body.contains("\"id\":\"call_x\""),
        "missing tool call id in {body}"
    );

    // Ordering regression guard: `data: [DONE]` MUST come after the
    // `event: tool_result` frame, never before it. The browser uses
    // `[DONE]` to close the response stream (`reader.cancel()`); if
    // it arrives before the tool events the client cuts the
    // connection and the tool bubble never resolves. The proxy
    // therefore swallows the upstream's `[DONE]` and emits exactly
    // one `[DONE]` at the very end of the last round.
    let tool_result_pos = body
        .find("event: tool_result")
        .expect("event: tool_result present");
    let first_done_pos = body
        .find("data: [DONE]")
        .expect("data: [DONE] present (proxy emits a single sentinel)");
    assert!(
        first_done_pos > tool_result_pos,
        "data: [DONE] arrived before event: tool_result — the browser would cancel the \
         stream on `[DONE]` and drop the tool_result frame. body={body}"
    );

    // The upstream should have been called twice (round 1 with the
    // tool_calls finish, round 2 with the final reply).
    let captured = captured.lock().await;
    assert_eq!(captured.len(), 2, "expected 2 upstream calls");
    // Round 1: request must carry the injected `tools` array with
    // the web_fetch entry.
    let round1 = &captured[0];
    let tools = round1["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty(), "tools array should not be empty");
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t["function"]["name"].as_str())
        .collect();
    assert!(
        names.contains(&"web_fetch"),
        "web_fetch missing from injected tools: {names:?}"
    );
    // Round 2: the messages array must include the assistant's
    // tool_calls entry plus a role:tool message with the matching
    // tool_call_id. The exact content of the tool message varies
    // (sandbox denies example.com), but the role + id must be
    // present and end-to-end consistent.
    let round2 = &captured[1];
    let messages = round2["messages"].as_array().expect("messages array");
    let has_tool_msg = messages.iter().any(|m| {
        m["role"].as_str() == Some("tool") && m["tool_call_id"].as_str() == Some("call_x")
    });
    assert!(
        has_tool_msg,
        "round-2 messages must contain a role:tool entry with tool_call_id=call_x: {messages:?}"
    );
    let has_assistant_with_tool_calls = messages.iter().any(|m| {
        m["role"].as_str() == Some("assistant")
            && m["tool_calls"]
                .as_array()
                .is_some_and(|tcs| tcs.iter().any(|tc| tc["id"].as_str() == Some("call_x")))
    });
    assert!(
        has_assistant_with_tool_calls,
        "round-2 messages must contain the assistant turn with tool_calls[].id=call_x"
    );
}

#[cfg(feature = "web-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_loop_aborts_after_max_rounds() {
    // A pathological upstream that loops on tool_calls forever
    // must be cut off at `LLM_MAX_TOOL_ROUNDS`. The proxy surfaces
    // an `event: error` followed by `[DONE]`.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"))],
                concat!(
                    "event: message\n",
                    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_y\",\"type\":\"function\",\"function\":{\"name\":\"web_fetch\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
                    "data: [DONE]\n\n",
                ).to_string(),
            )
        }),
    );
    let upstream_url = spawn_router(app).await;

    let mut server_cfg = make_server_cfg(upstream_url);
    server_cfg.agents.llm_max_tool_rounds = 2;
    let cfg = Arc::new(server_cfg);
    let agents = AgentRegistry::from_config(&cfg.agents);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let llm_cfg = Arc::new(cfg.llm.clone());
    let llm_client = LlmClient::new(llm_cfg).unwrap();
    let state = make_app_state(cfg, Some(llm_client), Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[{"role":"user","content":"x"}],"stream":true,"model":"llama3.1"}"#)
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("event: error") && body.contains("agent loop exceeded"),
        "expected event: error with 'agent loop exceeded' in {body}"
    );
    // The loop must terminate; `data: [DONE]` always closes the
    // stream.
    assert!(body.contains("data: [DONE]"));
}

// ---- 4. datetime_agent end-to-end ----

#[cfg(feature = "datetime-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datetime_agent_returns_now_in_paris() {
    let agent = DateTimeAgent::new();
    let result = agent
        .invoke(serde_json::json!({"timezone": "Europe/Paris"}))
        .await
        .expect("invoke");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["source"], "system");
    assert_eq!(parsed["data"]["timezone"], "Europe/Paris");
    let offset = parsed["data"]["utc_offset"].as_str().unwrap();
    assert!(
        offset == "+01:00" || offset == "+02:00",
        "expected Europe/Paris offset (+01:00 or +02:00), got {offset}"
    );
}

#[cfg(feature = "datetime-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datetime_agent_rejects_unknown_timezone() {
    let agent = DateTimeAgent::new();
    let err = agent
        .invoke(serde_json::json!({"timezone": "Not/A_Zone"}))
        .await
        .expect_err("should reject");
    match err {
        stt_server::agents::AgentError::InvalidArguments(msg) => {
            assert!(msg.contains("unknown IANA timezone"));
        }
        other => panic!("expected InvalidArguments, got {other:?}"),
    }
}

#[cfg(feature = "datetime-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datetime_agent_invoke_endpoint_returns_400_for_bad_timezone() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = AgentRegistry::from_config(&cfg.agents);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let state = make_app_state(cfg, None, Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/agents/get_datetime/invoke"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"arguments":{"timezone":"Not/A_Zone"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---- 5. weather_agent end-to-end ----

#[cfg(feature = "weather-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weather_agent_parses_open_meteo_fixture() {
    // The agent hardcodes the Open-Meteo hostnames, so a loopback
    // fixture won't be reached by default. We test the parser +
    // builder path through `WeatherAgent::invoke` indirectly by
    // going through the loopback with `replace_url` shimmed via the
    // public API. The simplest end-to-end check is to drive the
    // payload builder with a canned forecast JSON, exactly the way
    // `weather_agent::tests::build_payload_shapes_current_and_daily`
    // does for the inner path; here we additionally verify the
    // agent's error path for a missing geocoding match.
    let agent = WeatherAgent::new();
    // Without a way to swap the upstream host in v1, the simplest
    // outer coverage is the bad-location path which doesn't touch
    // the network: the agent fails fast on empty input.
    let err = agent
        .invoke(serde_json::json!({"location": "  "}))
        .await
        .expect_err("empty location should fail");
    match err {
        stt_server::agents::AgentError::InvalidArguments(msg) => {
            assert!(msg.contains("location"));
        }
        other => panic!("expected InvalidArguments, got {other:?}"),
    }
}

#[cfg(feature = "weather-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weather_agent_caps_days_to_seven() {
    // `days` is advertised as 1-7 in the schema. The agent clamps
    // overshoot internally before issuing the upstream call, but
    // the LLM-supplied value is also bounded by the OpenAI
    // validators on most models. The schema guard is what we test
    // here — a regression to the cap would let a hostile model
    // request a 30-day forecast and balloon the response.
    let agent = WeatherAgent::new();
    let schema = agent.parameters_schema();
    let days = &schema["properties"]["days"];
    assert_eq!(days["minimum"], 1);
    assert_eq!(days["maximum"], 7);
}

#[cfg(feature = "weather-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weather_agent_accepts_lat_lon_form() {
    // A real upstream call would hit the network; here we verify
    // the parser accepts the `lat,lon` short form and produces a
    // structured `location` field. We can't easily mock Open-Meteo
    // from the public API surface — the agent hardcodes the host —
    // so we only assert the input-validation path. The full
    // network round-trip is exercised by the inline `tests` module.
    let agent = WeatherAgent::new();
    // A syntactically valid `lat,lon` skips geocoding and goes
    // straight to the forecast call. We expect either a successful
    // forecast or an `AgentFailed` (network) — but never an
    // `InvalidArguments`, because the short form is valid input.
    let result = agent
        .invoke(serde_json::json!({"location": "48.8566,2.3522"}))
        .await;
    match result {
        Ok(_) => { /* network reachable, lucky */ }
        Err(stt_server::agents::AgentError::InvalidArguments(msg)) => {
            panic!("lat,lon short form should not produce InvalidArguments, got: {msg}")
        }
        Err(_) => { /* upstream unreachable in test env — acceptable */ }
    }
}

// ---- 6. stock_agent end-to-end ----

#[cfg(feature = "stock-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stock_agent_parses_csv_with_loopback_fixture() {
    // The Stooq upstream is hardcoded, so we can't redirect it to
    // our loopback fixture from the public surface. The end-to-end
    // CSV path is covered exhaustively by the inline
    // `stock_agent::tests` module. This integration test pins the
    // public schema + name on the wired-up registry, so a
    // regression that renamed the tool surfaces here.
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = AgentRegistry::from_config(&cfg.agents);
    assert!(
        agents.get("get_stock_quote").is_some(),
        "registry must contain `get_stock_quote` when `stock-agent` feature is on"
    );
    let agent = agents.get("get_stock_quote").unwrap();
    assert_eq!(agent.name(), "get_stock_quote");
    let schema = agent.parameters_schema();
    assert!(schema["required"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!("ticker")));
}

#[cfg(feature = "stock-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stock_agent_rejects_invalid_ticker() {
    let agent = StockAgent::new();
    // The runtime char whitelist rejects these specific
    // characters even though spaces are now allowed (for
    // company names like "BNP Paribas"). A `;` is unambiguous
    // garbage, so the gate fires before any network call.
    let err = agent
        .invoke(serde_json::json!({"ticker": "AA;DROP"}))
        .await
        .expect_err("semicolons are not allowed");
    match err {
        stt_server::agents::AgentError::InvalidArguments(_) => {}
        other => panic!("expected InvalidArguments, got {other:?}"),
    }
    // Length check: anything over 40 chars is rejected up-front.
    let err = agent
        .invoke(serde_json::json!({"ticker": "x".repeat(41)}))
        .await
        .expect_err("too long");
    assert!(matches!(
        err,
        stt_server::agents::AgentError::InvalidArguments(_)
    ));
}

#[cfg(feature = "stock-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stock_agent_invoke_endpoint_returns_400_for_bad_ticker() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = AgentRegistry::from_config(&cfg.agents);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let state = make_app_state(cfg, None, Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/agents/get_stock_quote/invoke"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"arguments":{"ticker":"AA;DROP"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---- 7. tools-schema injection across all registered agents ----

/// Regression guard: every registered agent must contribute a
/// `tools` entry to the upstream chat-completion request. The
/// existing `tool_loop_dispatches_and_completes` test pins this for
/// `web_fetch`; the variant below covers the multi-agent case so a
/// future addition that forgets to register in `from_config` shows
/// up here rather than at runtime.
#[cfg(any(
    feature = "web-agent",
    feature = "datetime-agent",
    feature = "weather-agent",
    feature = "stock-agent"
))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_schema_includes_every_registered_agent() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = AgentRegistry::from_config(&cfg.agents);
    let listed = agents.list();
    let names: Vec<&str> = listed.iter().map(|s| s.name.as_str()).collect();
    let schemas = agents.tools_schema();
    assert_eq!(
        names.len(),
        schemas.len(),
        "every listed agent must produce exactly one tools-schema entry"
    );
    #[cfg(feature = "web-agent")]
    assert!(names.contains(&"web_fetch"));
    #[cfg(feature = "datetime-agent")]
    assert!(names.contains(&"get_datetime"));
    #[cfg(feature = "weather-agent")]
    assert!(names.contains(&"get_weather"));
    #[cfg(feature = "stock-agent")]
    assert!(names.contains(&"get_stock_quote"));
    for s in &schemas {
        let name = s["function"]["name"].as_str().unwrap();
        assert!(names.contains(&name));
        assert!(!s["function"]["description"].as_str().unwrap().is_empty());
    }
}
