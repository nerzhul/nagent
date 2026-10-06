//! End-to-end tests for the LLM proxy.
//!
//! ## What this test asserts
//!
//! 1. `POST /v1/chat/completions` forwards SSE chunks verbatim, in
//! order, with the correct content type.
//! 2. `Authorization: Bearer <key>` is forwarded when
//! `OLLAMA_API_KEY` is set.
//! 3. `Authorization` is **not** forwarded when `OLLAMA_API_KEY` is
//! unset (the server never injects one).
//! 4. Non-2xx upstream surfaces the original status to the client.
//! 5. `LLM_ENABLED=false` returns 404 on both `/v1/*` routes.

use std::sync::{Arc, Once};
use std::time::Duration;

use axum::http::{header, HeaderValue, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use nagent_server::config::LlmConfig;
use nagent_server::http::build_router;
use nagent_server::llm::LlmClient;
use nagent_server::testing::app_state;
use serde_json::Value;
use tokio::net::TcpListener;

/// Spawn a mock upstream that records the incoming request and
/// returns a configurable SSE body. Returns the bound address.
async fn spawn_mock_upstream(
    body: String,
    on_request: Arc<tokio::sync::Mutex<Option<axum::http::HeaderMap>>>,
) -> String {
    // The closure must be `Clone` for axum's `Handler` bound, so any
    // state the handler needs lives behind `Arc` and is rebuilt per
    // request. The body string is `Clone`, which makes this simple.
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post({
                let on_request = Arc::clone(&on_request);
                let body = body.clone();
                move |headers: axum::http::HeaderMap, _body: axum::body::Bytes| {
                    let on_request = Arc::clone(&on_request);
                    let body = body.clone();
                    async move {
                        *on_request.lock().await = Some(headers);
                        (
                            StatusCode::OK,
                            [(
                                header::CONTENT_TYPE,
                                HeaderValue::from_static("text/event-stream"),
                            )],
                            body,
                        )
                    }
                }
            }),
        )
        .route(
            "/v1/models",
            get(|| async {
                let body = r#"{"object":"list","data":[{"id":"llama3.1","object":"model"}]}"#;
                (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    body,
                )
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Build a full stt-server with the LLM proxy enabled and pointing at
/// `upstream_url`.
async fn start_test_server_with_llm(
    upstream_url: String,
    api_key: Option<String>,
) -> (
    String,
    Arc<tokio::sync::Mutex<Option<axum::http::HeaderMap>>>,
) {
    start_test_server_with_llm_and_system_prompt(upstream_url, api_key, None).await
}

/// Same as [`start_test_server_with_llm`] but lets the caller set the
/// admin-configured system prompt (used by the new
/// `proxy_prepends_default_system_prompt_when_configured` /
/// `proxy_is_passthrough_when_system_prompt_unset` tests).
async fn start_test_server_with_llm_and_system_prompt(
    upstream_url: String,
    api_key: Option<String>,
    system_prompt: Option<String>,
) -> (
    String,
    Arc<tokio::sync::Mutex<Option<axum::http::HeaderMap>>>,
) {
    let llm_cfg = LlmConfig {
        enabled: true,
        base_url: upstream_url,
        default_model: "llama3.1".into(),
        api_key,
        inbound_auth_key: None,
        auth_mode: nagent_server::config::LlmAuthMode::Forward,
        request_timeout: Duration::from_secs(120),
        cors_allow_origins: vec![],
        system_prompt,
        allow_user_location: true,
        allow_user_timezone: true,
        allow_user_reply_language: true,
        allow_user_memory: true,
        llm_max_tool_rounds: 4,
        llm_max_auto_continues: 0,
        ollama_num_predict: None,
        ollama_num_ctx: None,
    };
    let llm_client = LlmClient::new(Arc::new(llm_cfg.clone()))
        .expect("LlmClient::new should succeed for test config");

    let on_request = Arc::new(tokio::sync::Mutex::new(None));

    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).llm = llm_cfg;
    let state = builder.with_llm(llm_client).build();

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    // Keep tx alive for the duration of the test.
    std::mem::forget(tx);

    (url, on_request)
}

/// Build a stt-server with `LLM_ENABLED=false`. The `/v1/*` routes
/// are never registered, so the chat view sees a clean 404.
async fn start_test_server_disabled() -> String {
    let state = app_state().build();

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    std::mem::forget(tx);
    url
}

/// Compose a SSE body string from the given events. Each event is
/// emitted as a complete `data: …\n\n` frame, terminated by the
/// OpenAI-compatible `data: [DONE]\n\n` sentinel.
fn sse_body(events: &[&str]) -> String {
    let mut body = String::new();
    for e in events {
        body.push_str(&format!("data: {e}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn compose_events() -> Vec<&'static str> {
    vec![
        r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":" world"},"finish_reason":null}]}"#,
        r#"{"id":"1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_sse_chunks_verbatim() {
    let events = compose_events();
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&events), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm(upstream_url, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[{"role":"user","content":"hi"}],"stream":true,"model":"llama3.1"}"#)
        .send()
        .await
        .expect("post chat");

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default(),
        "text/event-stream"
    );
    // Defeat buffering hint must be set.
    assert_eq!(
        resp.headers()
            .get("x-accel-buffering")
            .and_then(|v| v.to_str().ok()),
        Some("no")
    );

    let body = resp.text().await.expect("body");
    let expected = sse_body(&events);
    assert_eq!(body, expected);

    // Content-Type was forwarded.
    let captured_headers = captured.lock().await.take().expect("captured headers");
    let upstream_ct = captured_headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        upstream_ct.starts_with("application/json"),
        "upstream should see Content-Type: application/json, got `{upstream_ct}`"
    );

    // Authorization must NOT be forwarded when the server has no key.
    assert!(
        captured_headers.get(header::AUTHORIZATION).is_none(),
        "proxy must not inject an Authorization header when OLLAMA_API_KEY is unset"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_bearer_when_key_set() {
    let events = compose_events();
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&events), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm(upstream_url, Some("secret-token".into())).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.bytes().await;

    let captured_headers = captured.lock().await.take().expect("captured headers");
    let auth = captured_headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(auth, "Bearer secret-token");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_non_2xx_is_surfaced_verbatim() {
    // Mock upstream that always 404s with a JSON error body. The
    // shape is intentionally NOT the Ollama / OpenAI
    // model-not-found envelope, so the detector should leave it
    // alone and the proxy passes the body through verbatim.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(
            |_headers: axum::http::HeaderMap, _body: axum::body::Bytes| async {
                (
                    StatusCode::NOT_FOUND,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    r#"{"error":"model not found"}"#,
                )
            },
        ),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let upstream_url = format!("http://{addr}");
    let (url, _) = start_test_server_with_llm(upstream_url, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("model not found"),
        "upstream error body must be forwarded, got: {body}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_not_found_response_includes_available_models() {
    // Mock upstream that:
    // - 404s /v1/chat/completions with the Ollama-style
    // model-not-found envelope
    // - 200s /v1/models with a list of available model ids
    // The proxy should reshape the 404 into the friendly form with
    // `error.type == "model_not_found"`, `error.param == <name>` and
    // `error.available == [...]`.
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |_headers: axum::http::HeaderMap, _body: axum::body::Bytes| async {
                    (
                        StatusCode::NOT_FOUND,
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        )],
                        r#"{"error":{"message":"model 'llama3.1' not found","type":"not_found_error","param":null,"code":null}}"#,
                    )
                },
            ),
        )
        .route(
            "/v1/models",
            get(|| async {
                (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    )],
                    r#"{"object":"list","data":[{"id":"qwen2.5:14b"},{"id":"deepseek-r1:14b"}]}"#,
                )
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let upstream_url = format!("http://{addr}");
    let (url, _) = start_test_server_with_llm(upstream_url, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true,"model":"llama3.1"}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: Value = resp.json().await.expect("json body");
    let err = body.get("error").expect("error envelope");
    assert_eq!(
        err.get("type").and_then(|v| v.as_str()),
        Some("model_not_found"),
        "friendly type missing, got: {err}"
    );
    assert_eq!(
        err.get("param").and_then(|v| v.as_str()),
        Some("llama3.1"),
        "offending model name must round-trip"
    );
    let available = err
        .get("available")
        .and_then(|v| v.as_array())
        .expect("available array");
    let ids: Vec<&str> = available.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(ids, vec!["qwen2.5:14b", "deepseek-r1:14b"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_non_streaming_request() {
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&["ok"]), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm(upstream_url, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":false}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_returns_404_on_v1_routes() {
    let url = start_test_server_disabled().await;

    let chat = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .body("{}")
        .send()
        .await
        .expect("post chat");
    assert_eq!(chat.status(), StatusCode::NOT_FOUND);

    let models = reqwest::get(format!("{url}/v1/models"))
        .await
        .expect("get models");
    assert_eq!(models.status(), StatusCode::NOT_FOUND);
}

/// Mock upstream that captures the *request body* alongside the
/// headers so tests can assert on `messages[0]` after the proxy
/// prepends the admin-configured system prompt.
async fn spawn_capturing_upstream(
    body: String,
) -> (
    String,
    Arc<tokio::sync::Mutex<Option<axum::http::HeaderMap>>>,
    Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
) {
    let on_request = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let on_body = Arc::new(tokio::sync::Mutex::new(None::<serde_json::Value>));

    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let on_request = Arc::clone(&on_request);
            let on_body = Arc::clone(&on_body);
            let body = body.clone();
            move |headers: axum::http::HeaderMap, req_body: axum::body::Bytes| {
                let on_request = Arc::clone(&on_request);
                let on_body = Arc::clone(&on_body);
                let body = body.clone();
                async move {
                    *on_request.lock().await = Some(headers);
                    *on_body.lock().await =
                        Some(serde_json::from_slice(&req_body).unwrap_or(Value::Null));
                    (
                        StatusCode::OK,
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("text/event-stream"),
                        )],
                        body,
                    )
                }
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), on_request, on_body)
}

const SERVER_PROMPT: &str = "You are a strict, concise assistant.";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_prepends_default_system_prompt_when_configured() {
    let (_upstream_url, _on_headers, on_body) = spawn_capturing_upstream(sse_body(&["ok"])).await;
    let (chat_url, _) = start_test_server_with_llm_and_system_prompt(
        _upstream_url,
        None,
        Some(SERVER_PROMPT.into()),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{chat_url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[{"role":"user","content":"hi"}],"stream":true,"model":"llama3.1"}"#)
        .send()
        .await
        .expect("post chat");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.bytes().await;

    let captured = on_body.lock().await.take().expect("captured body");
    let messages = captured["messages"]
        .as_array()
        .expect("upstream received a `messages` array");
    assert_eq!(
        messages.len(),
        2,
        "admin system prompt + client user message, got: {captured}"
    );
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], SERVER_PROMPT);
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "hi");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_is_passthrough_when_system_prompt_unset() {
    // Backwards-compatibility guard: when the admin has not set
    // `LLM_SYSTEM_PROMPT` (and the field defaults to `None`), the
    // proxy must inject nothing. Without this assertion a future
    // regression could silently start adding a default system message
    // and break every deployment that relies on the passthrough.
    let (_upstream_url, _on_headers, on_body) = spawn_capturing_upstream(sse_body(&["ok"])).await;
    let (chat_url, _) =
        start_test_server_with_llm_and_system_prompt(_upstream_url, None, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{chat_url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[{"role":"user","content":"hi"}],"stream":true,"model":"llama3.1"}"#)
        .send()
        .await
        .expect("post chat");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.bytes().await;

    let captured = on_body.lock().await.take().expect("captured body");
    let messages = captured["messages"]
        .as_array()
        .expect("upstream received a `messages` array");
    assert_eq!(
        messages.len(),
        1,
        "no admin prompt → only the client message, got: {captured}"
    );
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "hi");
}

// ---- Ollama-native knobs (`ollama_num_predict`, `ollama_num_ctx`) ----------
//
// Mirrors the `start_test_server_with_llm_and_system_prompt` helper
// shape so we can exercise the `LLM_OLLAMA_NUM_PREDICT` and
// `LLM_OLLAMA_NUM_CTX` knobs end-to-end without depending on the
// system-prompt knob. The fields are passed through to the upstream
// body under `options.num_predict` / `options.num_ctx` respectively.

/// Same as [`start_test_server_with_llm_and_system_prompt`] but
/// lets the caller pick `ollama_num_predict` directly so the
/// body-merge path can be exercised end-to-end. Returns the chat
/// URL plus the upstream-side body capture (the only thing these
/// tests need to assert on).
async fn start_test_server_with_llm_ollama_predict(
    upstream_url: String,
    ollama_num_predict: Option<u32>,
) -> (String, Arc<tokio::sync::Mutex<Option<Value>>>) {
    let llm_cfg = LlmConfig {
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
        allow_user_memory: true,
        llm_max_tool_rounds: 4,
        llm_max_auto_continues: 0,
        ollama_num_predict,
        ollama_num_ctx: None,
    };
    let llm_client = LlmClient::new(Arc::new(llm_cfg.clone()))
        .expect("LlmClient::new should succeed for test config");

    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).llm = llm_cfg;
    let state = builder.with_llm(llm_client).build();

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    std::mem::forget(tx);

    let on_body = Arc::new(tokio::sync::Mutex::new(None));
    (url, on_body)
}

/// Same as [`start_test_server_with_llm_ollama_predict`] but for
/// the `ollama_num_ctx` knob. Kept distinct from the predict helper
/// even though they look symmetric so a future change to one knob
/// does not silently affect the other's tests.
async fn start_test_server_with_llm_ollama_ctx(
    upstream_url: String,
    ollama_num_ctx: Option<u32>,
) -> (String, Arc<tokio::sync::Mutex<Option<Value>>>) {
    let llm_cfg = LlmConfig {
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
        allow_user_memory: true,
        llm_max_tool_rounds: 4,
        llm_max_auto_continues: 0,
        ollama_num_predict: None,
        ollama_num_ctx,
    };
    let llm_client = LlmClient::new(Arc::new(llm_cfg.clone()))
        .expect("LlmClient::new should succeed for test config");

    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).llm = llm_cfg;
    let state = builder.with_llm(llm_client).build();

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    std::mem::forget(tx);

    let on_body = Arc::new(tokio::sync::Mutex::new(None));
    (url, on_body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_injects_ollama_num_predict_into_options_when_configured() {
    // The `LLM_OLLAMA_NUM_PREDICT` knob is opt-in: when set, the
    // proxy must merge `options.num_predict` into the upstream
    // body so reasoning models get a per-response generation
    // budget that lets them finish a reasoning + tool-call round
    // in one upstream request.
    let (upstream_url, _on_headers, on_body) = spawn_capturing_upstream(sse_body(&["ok"])).await;
    let (chat_url, _) = start_test_server_with_llm_ollama_predict(upstream_url, Some(2048)).await;

    let resp = reqwest::Client::new()
        .post(format!("{chat_url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[{"role":"user","content":"hi"}],"stream":true,"model":"llama3.1"}"#)
        .send()
        .await
        .expect("post chat");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.bytes().await;

    let captured = on_body.lock().await.take().expect("captured body");
    let options = captured["options"]
        .as_object()
        .expect("upstream received `options` object");
    assert_eq!(
        options["num_predict"].as_u64(),
        Some(2048),
        "expected `options.num_predict = 2048`, got: {captured}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_does_not_inject_ollama_num_predict_when_unset() {
    // Backwards-compatibility guard: when the admin has not set
    // `LLM_OLLAMA_NUM_PREDICT` (and the field defaults to
    // `None`), the proxy must NOT inject `options.num_predict` so
    // the upstream keeps its own default (e.g. Ollama's 128
    // tokens). Without this assertion a future regression could
    // silently start setting `num_predict` and break every
    // deployment that relies on the upstream default.
    let (upstream_url, _on_headers, on_body) = spawn_capturing_upstream(sse_body(&["ok"])).await;
    let (chat_url, _) = start_test_server_with_llm_ollama_predict(upstream_url, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{chat_url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[{"role":"user","content":"hi"}],"stream":true,"model":"llama3.1"}"#)
        .send()
        .await
        .expect("post chat");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.bytes().await;

    let captured = on_body.lock().await.take().expect("captured body");
    let injected = captured
        .get("options")
        .and_then(|o| o.get("num_predict"))
        .and_then(|v| v.as_u64());
    assert!(
        injected.is_none(),
        "expected no `options.num_predict` when knob is unset, got: {captured}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_injects_ollama_num_ctx_into_options_when_configured() {
    // The `LLM_OLLAMA_NUM_CTX` knob is opt-in: when set, the
    // proxy must merge `options.num_ctx` into the upstream body
    // so reasoning models see a context window large enough to
    // finish a reasoning + tool-call round in one upstream
    // request. Without a large enough `num_ctx` the Ollama
    // default of 2048 tokens cuts the model off mid-thought and
    // trips the `llm_max_auto_continues` heuristic.
    let (upstream_url, _on_headers, on_body) = spawn_capturing_upstream(sse_body(&["ok"])).await;
    let (chat_url, _) = start_test_server_with_llm_ollama_ctx(upstream_url, Some(32768)).await;

    let resp = reqwest::Client::new()
        .post(format!("{chat_url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[{"role":"user","content":"hi"}],"stream":true,"model":"llama3.1"}"#)
        .send()
        .await
        .expect("post chat");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.bytes().await;

    let captured = on_body.lock().await.take().expect("captured body");
    let options = captured["options"]
        .as_object()
        .expect("upstream received `options` object");
    assert_eq!(
        options["num_ctx"].as_u64(),
        Some(32768),
        "expected `options.num_ctx = 32768`, got: {captured}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_does_not_inject_ollama_num_ctx_when_unset() {
    // Backwards-compatibility guard: when the admin has not set
    // `LLM_OLLAMA_NUM_CTX` (and the field defaults to `None`),
    // the proxy must NOT inject `options.num_ctx` so the upstream
    // keeps its own default (e.g. a Modelfile `PARAMETER num_ctx
    // 32768`, or `OLLAMA_CONTEXT_LENGTH`). Without this assertion
    // a future regression could silently start setting `num_ctx`
    // and break every deployment that already pins it upstream.
    let (upstream_url, _on_headers, on_body) = spawn_capturing_upstream(sse_body(&["ok"])).await;
    let (chat_url, _) = start_test_server_with_llm_ollama_ctx(upstream_url, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{chat_url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[{"role":"user","content":"hi"}],"stream":true,"model":"llama3.1"}"#)
        .send()
        .await
        .expect("post chat");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.bytes().await;

    let captured = on_body.lock().await.take().expect("captured body");
    let injected = captured
        .get("options")
        .and_then(|o| o.get("num_ctx"))
        .and_then(|v| v.as_u64());
    assert!(
        injected.is_none(),
        "expected no `options.num_ctx` when knob is unset, got: {captured}"
    );
}

// ---- Inbound auth gate (P0 — Auth on the LLM proxy) ------------------------
//
// Mirrors the structure of `start_test_server_with_llm_and_system_prompt`
// but lets the caller pick `auth_mode` + `inbound_auth_key` directly so
// we can exercise the bearer path end-to-end without an upstream that
// cares about auth.

/// Same as [`start_test_server_with_llm_and_system_prompt`] but lets
/// the caller pick `auth_mode` + `inbound_auth_key` so the bearer
/// path can be exercised end-to-end. The function takes the LLM config
/// fields by value (rather than rebuilding the whole `ServerConfig`)
/// to keep the helper close to the existing one.
async fn start_test_server_with_llm_auth(
    upstream_url: String,
    auth_mode: nagent_server::config::LlmAuthMode,
    inbound_auth_key: Option<String>,
) -> (
    String,
    Arc<tokio::sync::Mutex<Option<axum::http::HeaderMap>>>,
) {
    let llm_cfg = LlmConfig {
        enabled: true,
        base_url: upstream_url,
        default_model: "llama3.1".into(),
        api_key: None,
        inbound_auth_key,
        auth_mode,
        request_timeout: Duration::from_secs(120),
        cors_allow_origins: vec![],
        system_prompt: None,
        allow_user_location: true,
        allow_user_timezone: true,
        allow_user_reply_language: true,
        allow_user_memory: true,
        llm_max_tool_rounds: 4,
        llm_max_auto_continues: 0,
        ollama_num_predict: None,
        ollama_num_ctx: None,
    };
    let llm_client = LlmClient::new(Arc::new(llm_cfg.clone()))
        .expect("LlmClient::new should succeed for test config");
    let on_request = Arc::new(tokio::sync::Mutex::new(None));

    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).llm = llm_cfg;
    let state = builder.with_llm(llm_client).build();

    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    std::mem::forget(tx);
    (url, on_request)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_mode_rejects_request_without_auth_header() {
    // `LLM_AUTH_MODE=bearer` + `LLM_API_KEY=…` → every `/v1/*` request
    // without the matching `Authorization: Bearer …` header is rejected
    // with `401 Unauthorized`. The upstream must never see the
    // request, which is what we're guarding against (P0 of the
    // security plan: LAN attacker driving the local LLM).
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&["ok"]), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm_auth(
        upstream_url,
        nagent_server::config::LlmAuthMode::Bearer,
        Some("swordfish".into()),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "missing Authorization header must produce 401"
    );
    // The standard `WWW-Authenticate: Bearer` hint must accompany
    // the 401 so SDKs and curl surface a useful error.
    assert_eq!(
        resp.headers()
            .get(header::WWW_AUTHENTICATE)
            .map(|v| v.to_str().unwrap().to_string()),
        Some("Bearer realm=\"nagent-llm-proxy\"".to_string()),
        "401 must include a WWW-Authenticate hint"
    );

    // The upstream must not have been contacted at all. The captured
    // header map stays `None` after a successful handler hit because
    // the spawn_mock_upstream helper only writes it from the inner
    // closure that runs when the upstream is hit. Asserting on that
    // gives us a stronger guarantee than a `is_success` check on the
    // upstream (we don't have one).
    assert!(
        captured.lock().await.is_none(),
        "upstream must never be contacted when auth fails"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_mode_rejects_wrong_key() {
    // Defence against a typo: a valid `Authorization: Bearer`
    // header with the wrong key must also be rejected. The check is
    // done in constant time so the comparison itself does not leak
    // the expected key length; this test guards the surface, the
    // constant-time property is exercised by the unit test in
    // `lib.rs`.
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&["ok"]), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm_auth(
        upstream_url,
        nagent_server::config::LlmAuthMode::Bearer,
        Some("swordfish".into()),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::AUTHORIZATION, "Bearer wrong-key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "wrong bearer key must produce 401"
    );
    assert!(captured.lock().await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_mode_accepts_matching_key() {
    // Happy path: the right key reaches the upstream. We assert on
    // the upstream captured header map to confirm the request
    // actually went through (and that no `Authorization` was added
    // by the proxy — `inbound_auth_key` is for inbound gating, the
    // outbound `api_key` stays unset for this test).
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&["ok"]), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm_auth(
        upstream_url,
        nagent_server::config::LlmAuthMode::Bearer,
        Some("swordfish".into()),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::AUTHORIZATION, "Bearer swordfish")
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.bytes().await;

    let captured_headers = captured.lock().await.take().expect("upstream was hit");
    assert!(
        captured_headers.get(header::AUTHORIZATION).is_none(),
        "the server-side outbound api_key is unset, so no Authorization \
         must reach the upstream. The inbound auth gate and the \
         outbound api_key are independent knobs."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_mode_ignores_inbound_auth_header() {
    // The default mode (`forward`) keeps the historical behaviour:
    // the proxy never inspects the inbound `Authorization` header and
    // forwards any value the client sent untouched (we don't add one
    // either, since `api_key` is unset).
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&["ok"]), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm_auth(
        upstream_url,
        nagent_server::config::LlmAuthMode::Forward,
        // Set the key too so we know the gate is the mode, not the
        // absence of the key.
        Some("swordfish".into()),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "forward mode must let unauthenticated requests through"
    );
    let _ = resp.bytes().await;
    let captured_headers = captured.lock().await.take().expect("upstream was hit");
    assert!(captured_headers.get(header::AUTHORIZATION).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_mode_is_noop_without_key() {
    // Misconfiguration guard: if the operator enabled `bearer` mode
    // but forgot to set the key, the server logs a warning at boot
    // (see `main.rs`) and lets every request through here so a
    // misconfigured deployment still functions. The alternative
    // (refuse all traffic) would brick the server until the config
    // is fixed.
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&["ok"]), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm_auth(
        upstream_url,
        nagent_server::config::LlmAuthMode::Bearer,
        None,
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "bearer mode without a key must fall open (warning logged separately)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bearer_mode_accepts_lowercase_scheme() {
    // RFC 7235 says the auth scheme is case-insensitive. Both
    // `Bearer …` and `bearer …` must work so a curl user can hand-
    // type either form.
    let captured = Arc::new(tokio::sync::Mutex::new(None::<axum::http::HeaderMap>));
    let upstream_url = spawn_mock_upstream(sse_body(&["ok"]), Arc::clone(&captured)).await;
    let (url, _) = start_test_server_with_llm_auth(
        upstream_url,
        nagent_server::config::LlmAuthMode::Bearer,
        Some("swordfish".into()),
    )
    .await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::AUTHORIZATION, "bearer swordfish")
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"messages":[],"stream":true}"#)
        .send()
        .await
        .expect("post");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "lowercase 'bearer' scheme must be accepted"
    );
}

// ---- Reasoning-truncation auto-continue contract ---------------------------
//
// Reasoning-capable models (qwen3.5 with reasoning on, DeepSeek-R1,
// o1/o3) can hit the upstream's per-request token cap before ever
// producing a visible `delta.content` answer — the upstream surfaces
// this as `finish_reason: "length"` with an empty content. The
// proxy's auto-continue heuristic appends a "please continue" user
// message and re-requests so the model can finish its thought, up
// to `llm_max_auto_continues` times. The contract is:
//
// - `llm_max_auto_continues = 0` (or reasoning-only truncation
//   fires N+1 times): the loop closes the stream with
//   `data: [DONE]`, no follow-up round, the reasoning text
//   already streamed stays visible in the chat UI's
//   `<details>` block, the user can ask for an explicit
//   continuation.
// - `llm_max_auto_continues > 0` and the upstream truncates: the
//   loop appends the "please continue" user message, opens a
//   fresh upstream connection, and forwards everything verbatim.
//   The chat UI accumulates the reasoning across all rounds in
//   the `<details>` block; once the model emits a visible
//   `delta.content` and ends with `finish_reason: "stop"`, the
//   loop closes with `[DONE]`.
// - `tool_calls` present → the tool-call branch takes over
//   (budgeted by `llm_max_tool_rounds`); auto-continue does
//   not fire on tool-call rounds.
//
// See `docs/llm-configuration.md` for the full contract and
// how to bump the upstream's `num_ctx` / `-c` so the auto-
// continue fallback rarely needs to fire in practice.

/// Spawn a mock upstream that always returns a reasoning-only
/// truncation (`finish_reason: "length"`, no `delta.content`).
/// The proxy must close the stream after this single response
/// when `llm_max_auto_continues = 0`, and after N+1 responses
/// when `llm_max_auto_continues = N`.
async fn spawn_always_truncates_upstream(
    captured: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
) -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let captured = Arc::clone(&captured);
            async move {
                let parsed: serde_json::Value = serde_json::from_slice(&body)
                    .unwrap_or_else(|_| {
                        serde_json::json!({
                            "_raw": String::from_utf8_lossy(&body).to_string()
                        })
                    });
                captured.lock().await.push(parsed);
                let body = "event: message\n\
                     data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning\":\"thinking hard\"},\"finish_reason\":null}]}\n\n\
                     event: message\ndata: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
                     data: [DONE]\n\n";
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// With `llm_max_auto_continues = 0`, reasoning-only truncation
/// closes the stream after a single round: no follow-up request,
/// no SSE error event, the user sees the reasoning text in the
/// chat UI's `<details>` block and can ask for a continuation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_loop_closes_on_truncation_when_auto_continues_disabled() {
    let captured: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let upstream_url = spawn_always_truncates_upstream(captured.clone()).await;
    // The default fixture sets `llm_max_auto_continues: 0`,
    // i.e. the heuristic is disabled.
    let (url, _) = start_test_server_with_llm(upstream_url, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(
            r#"{"messages":[{"role":"user","content":"think about it"}],"stream":true,"model":"llama3.1"}"#,
        )
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.expect("body");

    assert!(
        body.contains("thinking hard"),
        "reasoning text from the round must reach the client; got: {body}"
    );
    assert!(
        body.contains("data: [DONE]"),
        "stream must end with [DONE] after the truncation; got: {body}"
    );
    assert!(
        !body.contains("event: error") && !body.contains("\"type\":\"error\""),
        "truncation must not surface an SSE error event; got: {body}"
    );
    let calls = captured.lock().await.clone();
    assert_eq!(
        calls.len(),
        1,
        "auto-continue disabled → exactly one upstream call; got {}",
        calls.len()
    );
}

/// With `llm_max_auto_continues > 0` and the upstream truncating
/// on round 0, the loop appends a "please continue" user message
/// and re-requests. The new helper builds a server with
/// `llm_max_auto_continues = 3` and a mock upstream that
/// truncates on round 0 then produces a visible answer with
/// `finish_reason: "stop"` on round 1. The client must see the
/// reasoning from round 0, the answer from round 1, and the
/// stream must end with `[DONE]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_loop_auto_continues_on_reasoning_truncation_until_visible_answer() {
    let captured: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let captured = Arc::clone(&captured);
            move |_headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let captured = Arc::clone(&captured);
                async move {
                    let parsed: serde_json::Value =
                        serde_json::from_slice(&body).unwrap_or_else(|_| {
                            serde_json::json!({
                                "_raw": String::from_utf8_lossy(&body).to_string()
                            })
                        });
                    let round = captured.lock().await.len();
                    captured.lock().await.push(parsed);
                    let body = if round == 0 {
                        // Round 0: reasoning-only truncation.
                        "event: message\n\
                         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":\
                         {\"role\":\"assistant\",\"reasoning\":\"thinking hard\"},\
                         \"finish_reason\":null}]}\n\n\
                         event: message\ndata: {\"id\":\"1\",\"choices\":[{\"index\":0,\
                         \"delta\":{},\"finish_reason\":\"length\"}]}\n\n\
                         data: [DONE]\n\n"
                            .to_string()
                    } else {
                        // Round 1: visible answer + stop.
                        "event: message\n\
                         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":\
                         {\"role\":\"assistant\",\"content\":\"Done.\"},\
                         \"finish_reason\":null}]}\n\n\
                         event: message\ndata: {\"id\":\"1\",\"choices\":[{\"index\":0,\
                         \"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                         data: [DONE]\n\n"
                            .to_string()
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
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let upstream_url = format!("http://{addr}");

    // Build a server with `llm_max_auto_continues = 3` so the
    // single auto-continue round is well within the cap.
    let llm_cfg = LlmConfig {
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
        allow_user_memory: true,
        llm_max_tool_rounds: 8,
        llm_max_auto_continues: 3,
        ollama_num_predict: None,
        ollama_num_ctx: None,
    };
    let llm_client = LlmClient::new(Arc::new(llm_cfg.clone()))
        .expect("LlmClient::new should succeed for test config");
    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).llm = llm_cfg;
    let state = builder.with_llm(llm_client).build();
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    std::mem::forget(tx);

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(
            r#"{"messages":[{"role":"user","content":"think about it"}],"stream":true,"model":"llama3.1"}"#,
        )
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.expect("body");

    // The reasoning from round 0 must reach the client so the
    // chat UI can render it in the `<details>` block.
    assert!(
        body.contains("thinking hard"),
        "reasoning text from the truncated round must reach the client; got: {body}"
    );
    // The visible answer from round 1 must also reach the
    // client.
    assert!(
        body.contains("Done."),
        "visible answer from the auto-continue round must reach the client; got: {body}"
    );
    // The stream closes cleanly.
    assert!(
        body.contains("data: [DONE]"),
        "stream must end with [DONE] after the auto-continue; got: {body}"
    );
    assert!(
        !body.contains("event: error") && !body.contains("\"type\":\"error\""),
        "auto-continue within the cap must not surface an error; got: {body}"
    );

    // The auto-continue round 1 must carry a "please continue"
    // user message so the model knows to resume. Inspect the
    // second upstream call's body.
    let calls = captured.lock().await.clone();
    assert_eq!(
        calls.len(),
        2,
        "expected 1 original + 1 auto-continue = 2 rounds; got {}",
        calls.len()
    );
    let messages = calls[1]["messages"]
        .as_array()
        .expect("round-1 messages array");
    let last = messages.last().expect("messages non-empty");
    assert_eq!(
        last["role"], "user",
        "last message must be the auto-continue prompt"
    );
    let last_content = last["content"].as_str().unwrap_or("");
    assert!(
        last_content.contains("continue") || last_content.contains("finish"),
        "auto-continue user prompt must ask for a continuation; was `{last_content}`"
    );
    // The message just before the last must be the empty-content
    // assistant partial turn (the upstream saw the truncation
    // and we did not re-feed the reasoning as assistant
    // content).
    let second_last = &messages[messages.len() - 2];
    assert_eq!(second_last["role"], "assistant");
    assert!(
        second_last["content"].as_str().unwrap_or("").is_empty(),
        "assistant partial turn must carry no content (reasoning stays client-side)"
    );
}

// ---- Tool search + BM25 router round-trip (plan 1791317253718) ----------
//
// End-to-end test of the new discovery surface. The mock upstream
// pretends to be a tool-aware LLM that:
//   1. receives a request with the dynamic `tools=[]` (the test
//      asserts `search_tools` is present and `get_weather` is
//      pre-selected because the user message is weather-y);
//   2. emits a `search_tools` tool call so the proxy dispatches
//      it server-side (no upstream call for the dispatch);
//   3. then a second round calls `get_weather` by name, and the
//      proxy dispatches that one too.
//
// The test captures the upstream `tools=[]` on every round so
// the assertions can verify the discovered set stays live on
// round 2.

/// Mock upstream that drives the LLM/tool loop. Round 0: capture
/// the `tools=[]` then emit a `search_tools` call. Round 1:
/// capture the `tools=[]` then emit a `get_weather` call. The
/// final `finish_reason: "stop"` closes the stream.
async fn spawn_tool_search_round_trip_upstream(
    captured_bodies: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>,
    first_call: Arc<Once>,
) -> String {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let captured_bodies = Arc::clone(&captured_bodies);
            let first_call = Arc::clone(&first_call);
            async move {
                let parsed: serde_json::Value = serde_json::from_slice(&body)
                    .unwrap_or_else(|_| serde_json::json!({}));
                captured_bodies.lock().await.push(parsed.clone());

                // Decide which tool call to emit based on the
                // round: the first upstream request we see must
                // call `search_tools` (the LLM has the message
                // but no schema for the right agent yet); the
                // second must call `get_weather` (the LLM now
                // has the schema from `search_tools`).
                let mut is_first = false;
                first_call.call_once(|| {
                    is_first = true;
                });
                let sse_body = if is_first {
                    // Round 0: emit a `search_tools` call.
                    "event: message\n\
                     data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n\
                     data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"search_tools\",\"arguments\":\"{\\\"query\\\":\\\"weather\\\",\\\"top_k\\\":3}\"}}]},\"finish_reason\":null}]}\n\n\
                     data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                     data: [DONE]\n\n"
                } else {
                    // Round 1: emit a `get_weather` call by name.
                    "event: message\n\
                     data: {\"id\":\"2\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n\
                     data: {\"id\":\"2\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"location\\\":\\\"Paris\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
                     data: {\"id\":\"2\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                     data: [DONE]\n\n"
                };
                (
                    StatusCode::OK,
                    [(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("text/event-stream"),
                    )],
                    sse_body,
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

/// Helper that builds the test server with the LLM proxy + a real
/// agent registry (so `search_tools` and `get_weather` are
/// available). Mirrors `start_test_server_with_llm` but adds the
/// agent registry wiring.
async fn start_test_server_with_llm_and_agents(upstream_url: String) -> String {
    use nagent_agents::AgentConfigs;
    let llm_cfg = LlmConfig {
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
        allow_user_memory: true,
        llm_max_tool_rounds: 4,
        llm_max_auto_continues: 0,
        ollama_num_predict: None,
        ollama_num_ctx: None,
    };
    let llm_client = LlmClient::new(Arc::new(llm_cfg.clone()))
        .expect("LlmClient::new should succeed for test config");

    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).llm = llm_cfg;
    // Wire the agents crate's agent registry. Plan 1791317253718
    // — at minimum we need `search_tools` + `get_weather` + the
    // meta-agent. The full `AgentRegistry::from_config` walks
    // every per-feature descriptor; a slim build that compiles
    // without `all-agents` will produce an empty registry, and
    // the test will skip the assertions below. The test is
    // gated on the `all-agents` feature via the dev-dep in
    // `nagent-server`'s `Cargo.toml` so the registry is always
    // populated in CI.
    let cfgs = AgentConfigs::default();
    let pool = nagent_agents::egress::EgressPool::new();
    let registry = nagent_agents::AgentRegistry::from_config(&cfgs, true, &pool);
    let state = builder
        .with_llm(llm_client)
        .with_agents(registry.clone())
        .build();
    // The test asserts on the round-0 `tools=[]` shape, which
    // goes through `build_tools_for_round` in
    // `chat_completions`. With the router NOT wired on
    // `LlmState` (because the test builder does not run the
    // app's wiring helpers), the round-0 build returns just
    // `search_tools` (no pre-selection). The discovered-set
    // check on round 1 still works because the dispatch site
    // records `search_tools` into `DiscoveredTools` after the
    // server-side dispatch. For full router integration tests
    // we'd need a richer test harness that runs the app's
    // wiring helpers; this end-to-end test focuses on the
    // discoverable dispatch path, not on BM25 pre-selection.
    let app = build_router(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");

    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await;
    });
    std::mem::forget(tx);
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tool_search_round_trip() {
    // The mock upstream records the body of every request so we
    // can assert on the round-0 `tools=[]` shape (must include
    // `search_tools`) and on the round-1 `messages` history
    // (the assistant `tool_calls` + the `role: "tool"`
    // `search_tools` result + the assistant `tool_calls` for
    // `get_weather`).
    let captured_bodies: Arc<tokio::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let first_call = Arc::new(Once::new());
    let upstream_url =
        spawn_tool_search_round_trip_upstream(captured_bodies.clone(), first_call).await;
    let url = start_test_server_with_llm_and_agents(upstream_url).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(
            r#"{"messages":[{"role":"user","content":"What is the weather in Paris?"}],"stream":true,"model":"llama3.1"}"#,
        )
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.expect("body");
    // Stream must close cleanly with [DONE].
    assert!(
        body.contains("data: [DONE]"),
        "stream must end with [DONE]; got: {body}"
    );

    let bodies = captured_bodies.lock().await.clone();
    assert!(
        bodies.len() >= 2,
        "expected at least 2 upstream rounds (search_tools + get_weather), got {}",
        bodies.len()
    );

    // Round 0: the `tools=[]` must include `search_tools` (the
    // helper always prepends it). The BM25 router is not wired
    // in this test fixture, so router pre-selection does not
    // contribute; the only entry is `search_tools` itself.
    let round0_body0 = bodies[0].clone();
    let round0_tools = round0_body0["tools"].as_array().unwrap_or_else(|| {
        panic!("upstream received no `tools` array on round 0; body[0] = {round0_body0}")
    });
    let round0_names: Vec<&str> = round0_tools
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
        })
        .collect();
    assert!(
        round0_names
            .iter()
            .any(|n| *n == nagent_agents::SEARCH_TOOLS_NAME),
        "round 0 must include `search_tools` in tools=[]; got: {round0_names:?}"
    );

    // Round 1: the LLM was instructed to call `get_weather` on
    // round 1; the dispatch happens server-side (no upstream
    // call for the `get_weather` agent itself), but the
    // conversation history is forwarded. The captured body[1]
    // `messages` array should include the synthetic assistant
    // `tool_calls` for `search_tools` and the `role: "tool"`
    // result for `search_tools` (the previous round's
    // dispatch). The LLM in this round emits a `get_weather`
    // call; the tool loop dispatches it server-side and the
    // SSE stream surfaces it to the chat UI as an
    // `event: tool_call` with `name: "get_weather"`. We assert
    // on the chat-UI-side stream here so the test catches a
    // regression where the dispatch path silently no-ops.
    if let Some(round1_messages) = bodies.get(1).and_then(|b| b["messages"].as_array()) {
        let saw_search_tools_call = round1_messages.iter().any(|m| {
            m.get("role").and_then(|r| r.as_str()) == Some("assistant")
                && m.get("tool_calls")
                    .and_then(|tc| tc.as_array())
                    .map(|tcs| {
                        tcs.iter().any(|tc| {
                            tc.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                                == Some("search_tools")
                        })
                    })
                    .unwrap_or(false)
        });
        assert!(
            saw_search_tools_call,
            "round 1 messages must include an assistant tool_calls[] entry for `search_tools`"
        );
    }
    // The chat-UI-facing SSE stream must surface the
    // `get_weather` dispatch: the tool loop emits a synthetic
    // `event: tool_call` followed by an `event: tool_result`
    // when the LLM emits a tool call. Verify on the response
    // body captured earlier.
    let sse_seen_weather_call = body.contains("\"name\":\"get_weather\"");
    assert!(
        sse_seen_weather_call,
        "SSE stream must surface the get_weather tool_call event; got: {body}"
    );
}
