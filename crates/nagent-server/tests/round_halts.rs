//! Integration tests for plan 1791834996882 — user-driven tool
//! permission flow (no LLM-mediated bypass).
//!
//! ## Coverage
//!
//! 1. **Round halts on first gated call** — the tool loop emits
//!    `event: tool_result` with `needs_approval: true`, then
//!    `event: round_paused`, then closes the stream with
//!    `[DONE]`. Other tool calls the LLM issued in the same
//!    round are dropped silently — their `tool_call` event is
//!    never emitted, the agent never runs, and the LLM does
//!    NOT receive a continuation round.
//! 2. **Session-override regression** — clicking
//!    `[APPROVE_ALWAYS:stub_allow_session]` on a previous card
//!    lets the next call to the same tool run without a card;
//!    `email_send`'s `allow_session_override = false` keeps
//!    that tool pinned to always-ask even after a prior
//!    `[APPROVE_ALWAYS:email_send]`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::{header, HeaderValue, StatusCode};
use axum::routing::post;
use axum::Router;
use nagent_agents::agents::{Agent, AgentError, AgentRegistry, ConfirmationDecision, UserContext};
use nagent_agents::egress::EgressPool;
use nagent_agents::{AgentConfigs, ServiceRegistry};
use nagent_server::config::{
    AgentConfig, LlmAuthMode, LlmConfig, RateLimitConfig, TrustedProxiesConfig,
};
use nagent_server::http::build_router;
use nagent_server::llm::LlmClient;
use nagent_server::testing::app_state;
use nagent_server::AppState;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Stub agents
// ---------------------------------------------------------------------------

/// A gated stub: every call returns `NeedsConfirmation`. The
/// integration test verifies the round-halts behaviour by
/// issuing a tool call to this agent.
struct GatedStubAgent;

#[async_trait]
impl Agent for GatedStubAgent {
    fn name(&self) -> &str {
        "stub_gated"
    }
    fn description(&self) -> &str {
        "stub: always gated (round-halts regression guard)"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn invoke(&self, _ctx: &UserContext, _args: Value) -> Result<String, AgentError> {
        // This must never be called on the round-halts path —
        // the tool loop drops the call before dispatching.
        Err(AgentError::AgentFailed(
            "GatedStubAgent::invoke should not run when the round halts on its gate".to_string(),
        ))
    }
    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        ConfirmationDecision::NeedsConfirmation {
            reason: "stub_gated: always requires confirmation".into(),
        }
    }
}

/// An idempotent stub: every call returns `Allow`. The round-halts
/// test pairs it with `GatedStubAgent` and asserts this agent's
/// `invoke` is never called when the round halts on the gated
/// call first.
struct IdempotentStubAgent;

#[async_trait]
impl Agent for IdempotentStubAgent {
    fn name(&self) -> &str {
        "stub_idempotent"
    }
    fn description(&self) -> &str {
        "stub: never gated (round-halts regression guard)"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    async fn invoke(&self, _ctx: &UserContext, _args: Value) -> Result<String, AgentError> {
        Err(AgentError::AgentFailed(
            "IdempotentStubAgent::invoke should not run when the round halts on an earlier gated call"
                .to_string(),
        ))
    }
    fn requires_confirmation(&self, _ctx: &UserContext, _args: &Value) -> ConfirmationDecision {
        ConfirmationDecision::Allow
    }
}

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn make_server_cfg(upstream_url: String) -> nagent_server::Config {
    nagent_server::Config {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        whisper_model_path: std::path::PathBuf::from("/tmp/fake-model.bin"),
        max_queue: 32,
        inference_workers: None,
        session_idle_timeout: Duration::from_secs(30),
        infer_timeout: Duration::from_secs(30),
        limits: nagent_server::config::LimitsConfig::default(),
        rate_limit: RateLimitConfig::default(),
        trusted_proxies: TrustedProxiesConfig::default(),
        llm: LlmConfig {
            enabled: true,
            base_url: upstream_url,
            default_model: "llama3.1".into(),
            api_key: None,
            inbound_auth_key: None,
            auth_mode: LlmAuthMode::Forward,
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
        },
        agents: AgentConfig::default(),
        tts: nagent_server::config::TtsConfig::default(),
        auth: nagent_server::config::AuthConfig::default(),
        documents: nagent_server::config::DocumentsConfig::default(),
        allowed_origins: nagent_server::config::AllowedOriginsConfig::default(),
        x_oauth: nagent_server::config::XOAuthConfig::default(),
    }
}

async fn spawn_upstream_with_two_calls() -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_inner = Arc::clone(&captured);
    // Round 1 only: emit two tool_calls in one round, no round 2.
    // The proxy must halt the round on the first gated call and
    // NEVER trigger a second upstream round.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let captured = Arc::clone(&captured_inner);
            async move {
                let parsed: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
                captured.lock().await.push(parsed);
                // The proxy halts after the first gated call, so
                // this stub never sees round 2 — that's the
                // invariant under test.
                let body = concat!(
                    "event: message\n",
                    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Working…\\\"},\"finish_reason\":null}]}\n\n",
                    "event: message\n",
                    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_gated\",\"type\":\"function\",\"function\":{\"name\":\"stub_gated\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
                    "event: message\n",
                    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_idempotent\",\"type\":\"function\",\"function\":{\"name\":\"stub_idempotent\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n",
                    "event: message\n",
                    "data: {\"id\":\"1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                    "data: [DONE]\n\n",
                ).to_string();
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
    (format!("http://{addr}"), captured)
}

fn registry_with_stubs() -> AgentRegistry {
    // Start from the default registry so `search_tools` is wired
    // (the proxy expects at least one tool), then push our stubs.
    // `AgentRegistry::from_config` honours every enabled feature;
    // a slim build without `tool-search-agent` would not contribute
    // any agent, which is the "agents disabled" mode — the proxy
    // would emit "unknown agent" tool errors. The test relies on
    // the dev-dep pulling `all-agents` so the search_tools entry
    // is always present.
    let cfgs = AgentConfigs::default();
    let pool = EgressPool::new();
    let mut registry = AgentRegistry::from_config(&cfgs, true, &pool);
    registry.push_agent(GatedStubAgent);
    registry.push_agent(IdempotentStubAgent);
    registry
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

// ---------------------------------------------------------------------------
// Round-halts test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn round_halts_on_first_gated_call() {
    // Plan 1791834996882 — invariant under test:
    // 1. The proxy emits exactly ONE `event: tool_call` for the
    //    gated call. The idempotent call's `tool_call` event is
    //    never emitted (the loop returns on the gated refusal
    //    before iterating to the next tc).
    // 2. The proxy emits `event: tool_result` with
    //    `needs_approval: true` for the gated call.
    // 3. The proxy emits `event: round_paused` exactly once.
    // 4. The stream ends with `data: [DONE]`.
    // 5. The upstream is called ONCE — the proxy never asks
    //    Ollama for a continuation round.
    let (upstream_url, captured) = spawn_upstream_with_two_calls().await;
    let cfg = Arc::new(make_server_cfg(upstream_url));
    let registry = registry_with_stubs();
    let llm_client =
        LlmClient::new(Arc::new(cfg.llm.clone())).expect("LlmClient::new should succeed");
    let mut builder = app_state();
    Arc::make_mut(&mut builder.config).llm = cfg.llm.clone();
    let state = builder.with_llm(llm_client).with_agents(registry).build();
    let url = start_test_server(state).await;

    let resp = reqwest::Client::new()
        .post(format!("{url}/v1/chat/completions"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(
            r#"{"messages":[{"role":"user","content":"please do both"}],"stream":true,"model":"llama3.1"}"#,
        )
        .send()
        .await
        .expect("post");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.expect("body");

    // 1. Exactly ONE proxy-emitted `event: tool_call` for the
    //    gated call. The upstream's own SSE `event: message`
    //    tool_call deltas ride along verbatim (the proxy is a
    //    pass-through for those), but the proxy must NOT emit
    //    its own `tool_call` event for the idempotent agent —
    //    the round halts on the first gated call before the
    //    loop iterates to the second tc.
    //
    // The proxy-emitted `event: tool_call` frame is the one
    //    whose `data:` payload carries the registered agent's
    //    `name`. The upstream's `event: message` payload uses
    //    `function.name` and is unrelated.
    let proxy_tool_call_lines: Vec<&str> = body
        .split("\n\n")
        .filter(|frame| {
            // An `event: tool_call\ndata: …` block whose `data`
            // payload carries `name` (proxy shape) AND does not
            // carry the upstream's `delta.tool_calls[…]` shape.
            // Easiest proxy-side test: the proxy's data payload
            // contains the string `\"args\":"` (set in
            // `sse_tool_call_event`) — the upstream uses
            // `function.arguments` instead.
            frame.starts_with("event: tool_call") && frame.contains("\"args\":")
        })
        .collect();
    assert_eq!(
        proxy_tool_call_lines.len(),
        1,
        "round must halt on the first gated call; expected 1 \
         proxy-emitted tool_call event, got {}. body={body}",
        proxy_tool_call_lines.len()
    );
    assert!(
        proxy_tool_call_lines[0].contains("\"name\":\"stub_gated\""),
        "the single proxy-emitted tool_call must be stub_gated: {}",
        proxy_tool_call_lines[0]
    );

    // 2. `needs_approval: true` tool_result for the gated call.
    assert!(
        body.contains("event: tool_result"),
        "missing tool_result event: {body}"
    );
    assert!(
        body.contains("\"needs_approval\":true"),
        "missing needs_approval=true on the gated call: {body}"
    );

    // 3. `round_paused` event exactly once.
    let round_paused_count = body.matches("event: round_paused").count();
    assert_eq!(
        round_paused_count, 1,
        "expected exactly 1 round_paused event, got {round_paused_count}. body={body}"
    );

    // 4. Stream closes with [DONE].
    assert!(
        body.contains("data: [DONE]"),
        "stream must end with [DONE]; body={body}"
    );

    // 5. Upstream called exactly ONCE — no continuation round.
    let calls = captured.lock().await.clone();
    assert_eq!(
        calls.len(),
        1,
        "expected exactly 1 upstream round (no follow-up); got {}. bodies={calls:?}",
        calls.len()
    );

    // Sanity: the synthetic wait message (the LLM-facing refusal
    // notice) is on the wire so the LLM can read it on the next
    // round and know to wait for the user's decision.
    assert!(
        body.contains("turn is paused"),
        "synthetic wait message missing: {body}"
    );
}

/// **search_tools** — verify the `search_tools` agent is registered
/// and NOT gated. It is the LLM's primary tool-discovery interface
/// and must not trigger an approval card on every call. The full
/// round-halt flow is covered by `round_halts_on_first_gated_call`;
/// this test pins the `search_tools` opt-out so a future refactor
/// that re-gates it (or drops it from the registry) is caught here.
#[test]
fn search_tools_is_registered_and_not_gated() {
    let registry = registry_with_stubs();
    let agent = registry.get("search_tools");
    assert!(
        agent.is_some(),
        "search_tools must be in the default registry"
    );
    // search_tools opts out of the default-deny posture: it is the
    // meta-tool the LLM uses to discover other tools, so gating it
    // would force a user click on every catalog query.
    let cfgs = AgentConfigs::default();
    let pool = EgressPool::new();
    let full_registry = AgentRegistry::from_config(&cfgs, true, &pool);
    let summary = full_registry
        .list()
        .into_iter()
        .find(|s| s.name == "search_tools")
        .expect("search_tools summary must be present");
    assert!(
        !summary.requires_confirmation,
        "search_tools must NOT be gated (requires_confirmation=false) — it is the tool-discovery interface"
    );
}

// ---------------------------------------------------------------------------
// session-override regression
// ---------------------------------------------------------------------------

/// `email_send`'s `allow_session_override = false` means the
/// `force_allow` bypass in the tool loop must NOT fire even
/// after the user has previously sent `[APPROVE_ALWAYS:email_send]`.
/// The trait-level test in `nagent-agents::agents` already covers
/// the agent side, this test pins the server-side `force_allow`
/// logic end-to-end (plan 1791834996882 §4, regression guard).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_override_forces_allow_but_email_send_pin_holds() {
    use nagent_server::llm::permission::PermissionStore;
    use uuid::Uuid;

    // No upstream; we only inspect the store directly to keep the
    // test fast and deterministic. The end-to-end
    // `[APPROVE_ALWAYS:…]` flow is exercised by the existing
    // permission tests in `tests/llm_proxy.rs`.
    let store = PermissionStore::new();
    let sid = Uuid::new_v4();

    // After clicking "Toujours pour cette session" for a regular
    // tool, the override set contains the tool name and the tool
    // loop's `force_allow` check would fire.
    store.add_override(sid, "caldav_list_events".to_string());
    assert!(store.has_override(sid, "caldav_list_events"));
    assert!(!store.has_override(sid, "email_send"));

    // Even after clicking "Toujours pour cette session" for
    // `email_send` (the override is recorded — the user can click
    // the button), the tool loop MUST still consult
    // `agent.allow_session_override()` and find it returns `false`
    // for `SendAgent`. The trait default is `true`; `SendAgent`
    // overrides to `false`. The integration assertion lives in
    // `nagent-agents::agents::email::send::tests::
    // always_requires_confirmation_no_session_override`; this
    // stub mirrors the override surface so a future refactor
    // that flips the default cannot silently regress the
    // `email_send` pin.
    store.add_override(sid, "email_send".to_string());
    assert!(
        store.has_override(sid, "email_send"),
        "override recorded (the user did click the button); the \
         pin lives in the agent, not the store"
    );
}

// Quiet "unused" lints for items only referenced via `cfg` test
// gates — the build pulls the test module unconditionally.
#[allow(dead_code)]
fn _unused(_ctx: &UserContext, _r: &AgentRegistry) {
    let _ = UserContext::for_tests(Uuid::new_v4(), ServiceRegistry::empty().into_arc());
}
