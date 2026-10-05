//! End-to-end tests for the server-side chat agents.
//!
//! ## Coverage
//!
//! 1. `GET /v1/agents` returns the registered agents (when enabled)
//! and an empty list (when disabled).
//! 2. `POST /v1/agents/web_fetch/invoke` succeeds against a
//! loopback fixture server (the sandbox always allows `127.0.0.1`
//! through `WEB_FETCH_ALLOW_PUBLIC=true`), and rejects unknown
//! agents / invalid args with the right HTTP status.
//! 3. `POST /v1/chat/completions` injects the agent's `tools`
//! schema into the upstream request, and when the upstream emits
//! a `tool_calls` block the proxy runs the agent and emits the
//! expected `event: tool_call` / `event: tool_result` SSE frames
//! before completing the turn.
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
    feature = "stock-agent",
    feature = "calculate-agent",
    feature = "unit-convert-agent",
    feature = "wikipedia-agent",
    feature = "dictionary-agent"
))]

use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, HeaderValue, StatusCode};
use axum::routing::{get, post};
use axum::Router;
// stt_core is no longer imported here — the phase-5.B test
// builder creates the backend + worker pool internally.
#[cfg(feature = "calculate-agent")]
use nagent_agents::agents::calculate_agent::CalculateAgent;
#[cfg(feature = "datetime-agent")]
use nagent_agents::agents::datetime_agent::DateTimeAgent;
#[cfg(feature = "dictionary-agent")]
use nagent_agents::agents::dictionary_agent::DictionaryAgent;
#[cfg(feature = "stock-agent")]
use nagent_agents::agents::stock_agent::StockAgent;
#[cfg(feature = "unit-convert-agent")]
use nagent_agents::agents::unit_convert_agent::UnitConvertAgent;
#[cfg(feature = "weather-agent")]
use nagent_agents::agents::weather_agent::WeatherAgent;
#[cfg(feature = "web-agent")]
use nagent_agents::agents::web_fetch::WebFetchAgent;
#[cfg(feature = "wikipedia-agent")]
use nagent_agents::agents::wikipedia_agent::WikipediaAgent;
use nagent_agents::ServiceRegistry;
use nagent_server::agents::UserContext;
use nagent_server::agents::{Agent, AgentRegistry};
use nagent_server::config::{
    AgentConfig, DictionaryConfig, LlmConfig, RateLimitConfig, WeatherConfig, WebFetchConfig,
    WikipediaConfig,
};
use nagent_server::http::build_router;
use nagent_server::llm::LlmClient;
use nagent_server::stt::session::SessionMap;
use nagent_server::{AppState, Config as ServerConfig};
use tokio::net::TcpListener;

/// Build a fresh `UserContext` for tests that exercise agents which
/// ignore the context. Mirrors the `UserContext::for_tests` helper
/// already used inside the per-agent unit tests in the lib crate;
/// we can't `pub(crate)`-export that, so this is a thin duplicate
/// of the two-argument call.
fn test_ctx() -> UserContext {
    UserContext::for_tests(uuid::Uuid::new_v4(), Arc::new(ServiceRegistry::empty()))
}

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

/// Spawn a loopback WeatherAPI fixture that serves the same canned
/// body for every endpoint (forecast and history). Returns the base
/// URL the agent's `WeatherConfig::base_url` should point at — the
/// agent appends `/v1/forecast.json` / `/v1/history.json`.
#[cfg(feature = "weather-agent")]
async fn spawn_weatherapi_fixture(body: serde_json::Value) -> String {
    // The fixture decides its own status code from the payload
    // shape: a top-level `error` key becomes a 400 (matches
    // WeatherAPI's actual behaviour), anything else is a 200. We
    // clone once per route so each handler owns its own copy.
    let forecast_body = body.clone();
    let history_body = body.clone();
    let forecast_app = Router::new().route(
        "/v1/forecast.json",
        get(move || {
            let canned = forecast_body.clone();
            async move {
                if canned.get("error").is_some() {
                    (
                        StatusCode::BAD_REQUEST,
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        )],
                        canned.to_string(),
                    )
                } else {
                    (
                        StatusCode::OK,
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        )],
                        canned.to_string(),
                    )
                }
            }
        }),
    );
    let history_app = Router::new().route(
        "/v1/history.json",
        get(move || {
            let canned = history_body.clone();
            async move {
                (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    canned.to_string(),
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, forecast_app.merge(history_app)).await;
    });
    format!("http://{addr}")
}

fn make_app_state(
    server_cfg: Arc<ServerConfig>,
    llm_client: Option<LlmClient>,
    agents: Option<AgentRegistry>,
    _sessions: SessionMap,
) -> Arc<AppState> {
    // The `sessions` argument is kept for backwards compatibility
    // with existing test bodies; the phase-5.B builder manages its
    // own session map. Tests that need to assert on session state
    // reach into the returned `AppState.stt.sessions` instead.
    let _ = _sessions;
    let mut builder = nagent_server::testing::app_state();
    builder.config = server_cfg;
    if let Some(client) = llm_client {
        builder = builder.with_llm(client);
    }
    if let Some(registry) = agents {
        builder = builder.with_agents(registry);
    }
    builder.build()
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
        inference_workers: None,
        session_idle_timeout: Duration::from_secs(30),
        infer_timeout: Duration::from_secs(30),
        limits: nagent_server::config::LimitsConfig::default(),
        rate_limit: RateLimitConfig::default(),
        trusted_proxies: nagent_server::config::TrustedProxiesConfig::default(),
        llm: LlmConfig {
            enabled: true,
            base_url: upstream_url,
            default_model: "llama3.1".into(),
            api_key: None,
            inbound_auth_key: None,
            auth_mode: nagent_server::config::LlmAuthMode::Forward,
            request_timeout: Duration::from_secs(120),
            cors_allow_origins: vec![],
            system_prompt: None,
            allow_user_location: true,
            allow_user_timezone: true,
            allow_user_reply_language: true,
            llm_max_tool_rounds: 4,
        },
        agents: AgentConfig::default(),
        tts: nagent_server::config::TtsConfig::default(),
        auth: nagent_server::config::AuthConfig::default(),
        documents: nagent_server::config::DocumentsConfig::default(),
        allowed_origins: nagent_server::config::AllowedOriginsConfig::default(),
        x_oauth: nagent_server::config::XOAuthConfig::default(),
    }
}

// ---- 1. /v1/agents list ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agents_list_returns_web_fetch_when_feature_enabled() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
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
    #[cfg(feature = "calculate-agent")]
    assert!(
        names.contains(&"calculate"),
        "expected `calculate` in {names:?}"
    );
    #[cfg(feature = "unit-convert-agent")]
    assert!(
        names.contains(&"unit_convert"),
        "expected `unit_convert` in {names:?}"
    );
    #[cfg(feature = "wikipedia-agent")]
    assert!(
        names.contains(&"wikipedia"),
        "expected `wikipedia` in {names:?}"
    );
    #[cfg(feature = "dictionary-agent")]
    assert!(
        names.contains(&"dictionary"),
        "expected `dictionary` in {names:?}"
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
    let result = agent.invoke(&test_ctx(), args).await.expect("invoke");
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
        .invoke(&test_ctx(), args)
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
        .invoke(&test_ctx(), args)
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
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
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
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
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
/// arguments split across chunks), then `finish_reason:
/// "tool_calls"`. Captures the request body so the test can
/// assert the `tools` field was injected.
/// - Round 2: returns a normal text reply (no tool calls) with
/// `finish_reason: "stop"`. The proxy should forward this to the
/// client verbatim and exit.
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
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
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
    server_cfg.llm.llm_max_tool_rounds = 2;
    let cfg = Arc::new(server_cfg);
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
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
        .invoke(&test_ctx(), serde_json::json!({"timezone": "Europe/Paris"}))
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
        .invoke(&test_ctx(), serde_json::json!({"timezone": "Not/A_Zone"}))
        .await
        .expect_err("should reject");
    match err {
        nagent_server::agents::AgentError::InvalidArguments(msg) => {
            assert!(msg.contains("unknown IANA timezone"));
        }
        other => panic!("expected InvalidArguments, got {other:?}"),
    }
}

#[cfg(feature = "datetime-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn datetime_agent_invoke_endpoint_returns_400_for_bad_timezone() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
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
async fn weather_agent_parses_weatherapi_fixture() {
    // Point the agent at a loopback axum server that returns a
    // canned WeatherAPI-style response. The agent's base_url is
    // configurable through `WeatherConfig`, which is what makes
    // this kind of end-to-end test possible (the previous
    // Open-Meteo-based agent hardcoded its host and could not be
    // tested this way).
    let canned = serde_json::json!({
        "location": {
            "name": "Paris", "region": "Ile-de-France", "country": "France",
            "lat": 48.8566, "lon": 2.3522, "tz_id": "Europe/Paris",
            "localtime": "2026-09-26T14:55"
        },
        "current": {
            "last_updated": "2026-09-26T14:30", "temp_c": 18.4,
            "feelslike_c": 17.2, "humidity": 65, "wind_kph": 12.1,
            "wind_dir": "NW", "pressure_mb": 1015.0, "uv": 4.0,
            "condition": {"text": "Partly cloudy", "code": 1003}
        },
        "forecast": {"forecastday": [{
            "date": "2026-09-26",
            "day": {"mintemp_c": 12.0, "maxtemp_c": 19.0, "avgtemp_c": 15.5,
                     "avghumidity": 65.0, "totalprecip_mm": 0.5,
                     "daily_chance_of_rain": 30, "daily_chance_of_snow": 0,
                     "maxwind_kph": 22.0, "uv": 4.0,
                     "condition": {"text": "Partly cloudy", "code": 1003}},
            "astro": {"sunrise": "07:42 AM", "sunset": "07:30 PM",
                      "moonrise": "10:14 PM", "moonset": "09:55 AM",
                      "moon_phase": "Waxing Gibbous", "moon_illumination": "78%"}
        }]}
    });
    let base_url = spawn_weatherapi_fixture(canned).await;
    let agent = WeatherAgent::new(WeatherConfig {
        api_key: "test-key".into(),
        timeout_ms: 2_000,
        base_url,
    });
    let result = agent
        .invoke(&test_ctx(), serde_json::json!({"location": "Paris"}))
        .await
        .expect("invoke");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["source"], "weatherapi.com");
    assert_eq!(parsed["data"]["location"]["name"], "Paris");
    assert_eq!(parsed["data"]["current"]["temp_c"], 18.4);
    assert_eq!(parsed["data"]["current"]["feels_like_c"], 17.2);
    assert_eq!(parsed["data"]["forecast"][0]["chance_of_rain"], 30);
    assert_eq!(parsed["data"]["astronomy"]["sunset"], "07:30 PM");
}

#[cfg(feature = "weather-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weather_agent_surfaces_upstream_error_message() {
    // WeatherAPI returns errors as
    // `{"error":{"code":N,"message":"…"}}`. The agent must
    // extract the message rather than dump the raw body so the
    // LLM sees actionable text.
    let canned = serde_json::json!({
        "error": {"code": 1006, "message": "No matching location found."}
    });
    let base_url = spawn_weatherapi_fixture(canned).await;
    let agent = WeatherAgent::new(WeatherConfig {
        api_key: "test-key".into(),
        timeout_ms: 2_000,
        base_url,
    });
    let err = agent
        .invoke(&test_ctx(), serde_json::json!({"location": "Atlantis"}))
        .await
        .expect_err("upstream error should surface");
    match err {
        nagent_server::agents::AgentError::Upstream { status, body } => {
            assert_eq!(status, 400);
            assert!(
                body.contains("No matching location found"),
                "expected extracted error message, got: {body}"
            );
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[cfg(feature = "weather-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weather_agent_caps_days_to_fourteen() {
    // The schema guard exists so the LLM cannot request a
    // 30-day forecast and balloon the response. The cap moved
    // from 7 (Open-Meteo) to 14 (WeatherAPI free tier).
    let agent = WeatherAgent::new(WeatherConfig {
        api_key: "test-key".into(),
        ..Default::default()
    });
    let schema = agent.parameters_schema();
    let days = &schema["properties"]["days"];
    assert_eq!(days["minimum"], 1);
    assert_eq!(days["maximum"], 14);
}

#[cfg(feature = "weather-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weather_agent_missing_api_key_is_a_clear_error() {
    // The most common deployment failure. The agent must point
    // at the signup URL rather than 401-ing confusingly.
    let agent = WeatherAgent::new(WeatherConfig::default());
    let err = agent
        .invoke(&test_ctx(), serde_json::json!({"location": "Paris"}))
        .await
        .expect_err("missing key should fail");
    match err {
        nagent_server::agents::AgentError::AgentFailed(msg) => {
            assert!(msg.contains("WEATHER_API_KEY"));
            assert!(msg.contains("weatherapi.com"));
        }
        other => panic!("expected AgentFailed with config hint, got {other:?}"),
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
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
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
        .invoke(&test_ctx(), serde_json::json!({"ticker": "AA;DROP"}))
        .await
        .expect_err("semicolons are not allowed");
    match err {
        nagent_server::agents::AgentError::InvalidArguments(_) => {}
        other => panic!("expected InvalidArguments, got {other:?}"),
    }
    // Length check: anything over 40 chars is rejected up-front.
    let err = agent
        .invoke(&test_ctx(), serde_json::json!({"ticker": "x".repeat(41)}))
        .await
        .expect_err("too long");
    assert!(matches!(
        err,
        nagent_server::agents::AgentError::InvalidArguments(_)
    ));
}

#[cfg(feature = "stock-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stock_agent_invoke_endpoint_returns_400_for_bad_ticker() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
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

// ---- 7. calculate_agent end-to-end ----

#[cfg(feature = "calculate-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calculate_agent_evaluates_arithmetic() {
    let agent = CalculateAgent::new();
    let result = agent
        .invoke(
            &test_ctx(),
            serde_json::json!({"expression": "15*87.5/100"}),
        )
        .await
        .expect("invoke");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["source"], "local");
    let v = parsed["data"]["value"].as_f64().unwrap();
    assert!((v - 13.125).abs() < 1e-9);
}

#[cfg(feature = "calculate-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calculate_agent_invoke_endpoint_returns_400_for_invalid_chars() {
    // End-to-end: a bad expression hits the deny-list at the parser
    // gate, the agent returns `InvalidArguments`, and the proxy
    // surfaces it as 400.
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let state = make_app_state(cfg, None, Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/agents/calculate/invoke"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"arguments":{"expression":"1; rm -rf /"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---- 8. unit_convert_agent end-to-end ----

#[cfg(feature = "unit-convert-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unit_convert_agent_converts_miles_to_km() {
    // The canonical chat-user case. 12 mi × 1.609344 = 19.312128 km.
    let agent = UnitConvertAgent::new(nagent_server::config::UnitConvertConfig::default());
    let result = agent
        .invoke(
            &test_ctx(),
            serde_json::json!({"value": 12, "from": "mile", "to": "km"}),
        )
        .await
        .expect("invoke");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["source"], "local");
    let v = parsed["data"]["value"].as_f64().unwrap();
    assert!((v - 19.312128).abs() < 1e-6);
    assert_eq!(parsed["data"]["category"], "length");
}

#[cfg(feature = "unit-convert-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unit_convert_agent_rejects_cross_category() {
    // The endpoint must surface `InvalidArguments` as 400, not as a
    // 500. The proxy relies on this distinction to tell the LLM
    // "your args are wrong" vs "the upstream broke".
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let state = make_app_state(cfg, None, Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/agents/unit_convert/invoke"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"arguments":{"value":1,"from":"km","to":"kg"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---- 9. wikipedia_agent end-to-end ----

/// Spawn a loopback fixture that serves a canned summary response
/// for every `/page/summary/{title}` URL. The agent's
/// `WikipediaConfig::base_url` is pointed at this server.
#[cfg(feature = "wikipedia-agent")]
async fn spawn_wikipedia_summary_fixture(body: serde_json::Value) -> String {
    let app = Router::new().route(
        "/api/rest_v1/page/summary/:title",
        get(move || {
            let canned = body.clone();
            async move {
                (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    canned.to_string(),
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[cfg(feature = "wikipedia-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wikipedia_agent_parses_summary_fixture() {
    let canned = serde_json::json!({
        "type": "standard",
        "title": "Lyon",
        "displaytitle": "Lyon",
        "extract": "Lyon is a city in France.",
        "description": "city in France",
        "content_urls": {
            "desktop": {"page": "https://en.wikipedia.org/wiki/Lyon"},
            "mobile": {"page": "https://en.wikipedia.org/wiki/Lyon"}
        }
    });
    let base_url = spawn_wikipedia_summary_fixture(canned).await;
    let agent = WikipediaAgent::new(WikipediaConfig {
        base_url: format!("{base_url}/api/rest_v1"),
        timeout_ms: 2_000,
        user_agent: "nagent-test/0.1".into(),
    });
    let result = agent
        .invoke(&test_ctx(), serde_json::json!({"title": "Lyon"}))
        .await
        .expect("invoke");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["source"], "wikipedia.org");
    assert_eq!(parsed["data"]["title"], "Lyon");
    assert_eq!(parsed["data"]["extract"], "Lyon is a city in France.");
    assert_eq!(parsed["data"]["description"], "city in France");
    assert_eq!(parsed["data"]["url"], "https://en.wikipedia.org/wiki/Lyon");
}

#[cfg(feature = "wikipedia-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wikipedia_agent_sends_descriptive_user_agent() {
    // Wikimedia rejects unidentified clients. We capture the inbound
    // `User-Agent` header on the fixture and assert it matches the
    // configured value verbatim — a regression that drops the
    // header would otherwise be invisible until production.
    use axum::extract::Request;

    let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured_clone = captured.clone();
    let app = Router::new().route(
        "/api/rest_v1/page/summary/:title",
        get(move |req: Request| {
            let captured = captured_clone.clone();
            async move {
                let ua = req
                    .headers()
                    .get(header::USER_AGENT)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                *captured.lock().unwrap() = ua;
                let body = serde_json::json!({
                    "title": "Lyon",
                    "extract": "stub",
                    "description": "stub",
                    "content_urls": {"desktop": {"page": "https://example/wiki/Lyon"}}
                });
                (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    body.to_string(),
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base_url = format!("http://{addr}/api/rest_v1");

    let agent = WikipediaAgent::new(WikipediaConfig {
        base_url,
        timeout_ms: 2_000,
        user_agent: "nagent-test-wikipedia/42 (+https://example.test)".into(),
    });
    let _ = agent
        .invoke(&test_ctx(), serde_json::json!({"title": "Lyon"}))
        .await
        .unwrap();
    let sent = captured.lock().unwrap().clone();
    assert_eq!(
        sent.as_deref(),
        Some("nagent-test-wikipedia/42 (+https://example.test)"),
        "User-Agent header must be sent verbatim from the config"
    );
}

#[cfg(feature = "wikipedia-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wikipedia_agent_invoke_endpoint_returns_400_for_missing_title() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let state = make_app_state(cfg, None, Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/agents/wikipedia/invoke"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"arguments":{}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---- 10. dictionary_agent end-to-end ----

/// Spawn a loopback fixture that serves a canned dictionary entry
/// for every `/entries/en/{word}` URL. The agent's
/// `DictionaryConfig::base_url` is pointed at this server.
#[cfg(feature = "dictionary-agent")]
async fn spawn_dictionary_entries_fixture(body: serde_json::Value) -> String {
    let app = Router::new().route(
        "/api/v2/entries/en/:word",
        get(move || {
            let canned = body.clone();
            async move {
                (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    canned.to_string(),
                )
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[cfg(feature = "dictionary-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dictionary_agent_parses_entries_fixture() {
    // Mirrors the upstream payload for "hello".
    let canned = serde_json::json!([{
        "word": "hello",
        "phonetics": [
            {"text": "/həˈləʊ/", "audio": ""},
            {"text": "", "audio": ""}
        ],
        "meanings": [{
            "partOfSpeech": "interjection",
            "definitions": [
                {
                    "definition": "A greeting said when meeting someone.",
                    "example": "Hello, everyone.",
                    "synonyms": [],
                    "antonyms": []
                }
            ],
            "synonyms": ["greeting"],
            "antonyms": []
        }],
        "license": {"name": "CC BY-SA 3.0"}
    }]);
    let base_url = spawn_dictionary_entries_fixture(canned).await;
    let agent = DictionaryAgent::new(DictionaryConfig {
        base_url: format!("{base_url}/api/v2"),
        timeout_ms: 2_000,
    });
    let result = agent
        .invoke(&test_ctx(), serde_json::json!({"word": "hello"}))
        .await
        .expect("invoke");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["source"], "dictionaryapi.dev");
    assert_eq!(parsed["data"]["word"], "hello");
    assert_eq!(parsed["data"]["phonetic"], "/həˈləʊ/");
    assert_eq!(
        parsed["data"]["meanings"][0]["definitions"][0]["definition"],
        "A greeting said when meeting someone."
    );
    assert_eq!(
        parsed["data"]["meanings"][0]["definitions"][0]["example"],
        "Hello, everyone."
    );
    assert_eq!(parsed["data"]["synonyms"][0], "greeting");
}

#[cfg(feature = "dictionary-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dictionary_agent_invoke_endpoint_returns_400_for_missing_word() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let state = make_app_state(cfg, None, Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/agents/dictionary/invoke"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"arguments":{}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---- 10. tools-schema injection across all registered agents ----

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
    feature = "stock-agent",
    feature = "calculate-agent",
    feature = "unit-convert-agent",
    feature = "wikipedia-agent",
    feature = "dictionary-agent"
))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_schema_includes_every_registered_agent() {
    let cfg = Arc::new(make_server_cfg("http://127.0.0.1:1".into()));
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
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
    #[cfg(feature = "calculate-agent")]
    assert!(names.contains(&"calculate"));
    #[cfg(feature = "unit-convert-agent")]
    assert!(names.contains(&"unit_convert"));
    #[cfg(feature = "wikipedia-agent")]
    assert!(names.contains(&"wikipedia"));
    #[cfg(feature = "dictionary-agent")]
    assert!(names.contains(&"dictionary"));
    for s in &schemas {
        let name = s["function"]["name"].as_str().unwrap();
        assert!(names.contains(&name));
        assert!(!s["function"]["description"].as_str().unwrap().is_empty());
    }
}

// ---- 11. reasoning-only truncation auto-continue (plan R8 follow-up) ----
//
// Regression guard for the "bubble stuck on reasoning" UX bug
// (qwen3.5 with reasoning on, DeepSeek-R1, …). The upstream
// emits a long stream of `delta.reasoning` chunks followed by
// `finish_reason: "length"` with no `delta.content`. Without
// auto-continue the client sees an empty bubble + an italic
// "[reasoning only — no answer received]" note. With the
// auto-continue heuristic in `run_tool_loop`, the proxy opens a
// follow-up round asking the model to finish the answer and the
// client receives the visible reply as a continuation of the
// same SSE stream.

/// Fake upstream modelling a `finish_reason: "length"` round
/// followed by a successful continuation:
///
/// - Round 1: emits `delta.role: "assistant"` + a long
///   `delta.reasoning` chain and ends with
///   `finish_reason: "length"` (no content).
/// - Round 2: emits the visible `delta.content` answer and ends
///   with `finish_reason: "stop"`.
///
/// Round-2 captures the request body so the test can assert the
/// auto-continue appended a "please continue" user message and the
/// partial assistant turn.
#[cfg(feature = "web-agent")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_loop_auto_continues_after_reasoning_truncation() {
    use std::sync::Arc;
    let captured: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let upstream_url = spawn_reasoning_truncation_upstream(captured.clone()).await;

    let cfg = Arc::new(make_server_cfg(upstream_url));
    let agents = nagent_server::agents::build_registry(&cfg.agents, None, None);
    let sessions: SessionMap = Arc::new(dashmap::DashMap::new());
    let llm_cfg = Arc::new(cfg.llm.clone());
    let llm_client = LlmClient::new(llm_cfg).unwrap();
    let state = make_app_state(cfg, Some(llm_client), Some(agents), sessions);
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(
            r#"{"messages":[{"role":"user","content":"answer me"}],"stream":true,"model":"qwen3.5"}"#,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = resp.text().await.unwrap();
    // The continuation round must surface the visible answer to
    // the client verbatim. If the auto-continue logic didn't
    // fire, the body would only contain the reasoning chain
    // and `[DONE]` without the answer.
    assert!(
        body.contains("Done."),
        "client did not receive the visible answer (auto-continue did not fire?): {body}"
    );

    // Round-2 (the continuation) must have appended a user
    // message asking the model to finish, plus an empty-content
    // assistant partial turn so the upstream sees the
    // conversation history correctly.
    let captured = captured.lock().await;
    assert_eq!(
        captured.len(),
        2,
        "expected 2 rounds (initial + auto-continue), got {}",
        captured.len()
    );
    let messages = captured[1]["messages"]
        .as_array()
        .expect("round-2 messages array");
    // The last message must be the auto-continue user prompt.
    let last = messages.last().expect("messages array non-empty");
    let last_role = last["role"].as_str().unwrap_or("");
    let last_content = last["content"].as_str().unwrap_or("");
    assert_eq!(
        last_role, "user",
        "last message must be the auto-continue user prompt"
    );
    assert!(
        last_content.contains("continue") || last_content.contains("finish"),
        "auto-continue user prompt must ask for a continuation (was `{last_content}`)"
    );
    // The message just before the last must be the empty-content
    // assistant partial turn.
    let second_last = &messages[messages.len() - 2];
    let second_last_role = second_last["role"].as_str().unwrap_or("");
    assert_eq!(
        second_last_role, "assistant",
        "second-to-last message must be the assistant partial turn"
    );
    let second_last_content = second_last["content"].as_str().unwrap_or("");
    assert!(
        second_last_content.is_empty(),
        "assistant partial turn must carry no content (was `{second_last_content}`)"
    );
}

/// Fake upstream that emits a reasoning-only truncation on round 1
/// and a normal text reply on round 2.
#[cfg(feature = "web-agent")]
async fn spawn_reasoning_truncation_upstream(
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
                    // Round 1: reasoning-only truncation. The
                    // chain below mirrors the user-reported payload
                    // from qwen3.5 with reasoning on.
                    let mut s = String::new();
                    s.push_str("event: message\n");
                    s.push_str(
                        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning\":\"Let me think.\"},\"finish_reason\":null}]}\n\n",
                    );
                    for token in [" user", " wants", " an", " answer", "."] {
                        s.push_str("event: message\n");
                        s.push_str(&format!(
                            "data: {{\"id\":\"1\",\"choices\":[{{\"index\":0,\"delta\":{{\"reasoning\":\"{token}\"}},\"finish_reason\":null}}]}}\n\n"
                        ));
                    }
                    s.push_str(
                        "event: message\ndata: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
                    );
                    s.push_str("data: [DONE]\n\n");
                    s
                } else {
                    // Round 2: visible answer after auto-continue.
                    concat!(
                        "event: message\n",
                        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Done.\"},\"finish_reason\":null}]}\n\n",
                        "event: message\n",
                        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
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
