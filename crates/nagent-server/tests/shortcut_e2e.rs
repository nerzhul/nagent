//! End-to-end test for the Shortcut plugin (plan 1795597719369).
//!
//! Focused on the cross-cutting concerns the unit tests in
//! `nagent-agents` cannot exercise:
//!
//! 1. `GET /v1/agents` lists exactly the ten `shortcut_*` tools
//!    when the `shortcut-agent` feature is on.
//! 2. The `shortcut` `ServiceDef` shape (API token password field
//!    + probe-filled `username`, both never echoed on edit).
//! 3. The full setup flow against a loopback Shortcut API
//!    fixture: `PUT /api/integrations/shortcut/credentials` (the
//!    token into the vault), `POST /api/integrations/shortcut/probe`
//!    (`GET /members` list round-trip → member matched by mention
//!    name), then `POST /v1/agents/shortcut_list_backlog/invoke`
//!    against the same fixture.
//! 4. AGENTS.md §6: the token never appears in any HTTP response
//!    body (probe error included).

#![cfg(feature = "shortcut-agent")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use tower::ServiceExt;
use uuid::Uuid;

const TEST_TOKEN: &str = "sc_e2e_token_0123456789abcdef";

/// The ten tool names registered by the `shortcut-agent` feature.
const SHORTCUT_TOOLS: &[&str] = &[
    "shortcut_me",
    "shortcut_list_workflows",
    "shortcut_list_projects",
    "shortcut_list_epics",
    "shortcut_get_epic",
    "shortcut_list_iterations",
    "shortcut_search_stories",
    "shortcut_list_backlog",
    "shortcut_get_story",
    "shortcut_list_labels",
];

async fn connect_db(cfg: &nagent_server::config::AuthConfig) -> nagent_db::Db {
    let opts: nagent_db::DbOptions = cfg.into();
    nagent_db::Db::connect(&opts).await.expect("store connects")
}

async fn temp_store() -> nagent_db::Db {
    let cfg = nagent_server::config::AuthConfig {
        enabled: true,
        backends: vec![nagent_server::config::AuthBackendKind::Local],
        public_url: "https://example.com".into(),
        session_ttl_days: 7,
        csrf_header: "x-csrf-token".into(),
        db: nagent_server::config::AuthDbConfig {
            backend: "sqlite".into(),
            url: format!(
                "sqlite://file:shortcut_e2e_{}?mode=memory&cache=shared",
                Uuid::new_v4()
            ),
            max_connections: 1,
            auto_migrate: true,
        },
        ..Default::default()
    };
    let store = connect_db(&cfg).await;
    store.migrate().await.expect("migrate");
    store
}

// ---------------------------------------------------------------------------
// Loopback Shortcut API fixture (hand-rolled HTTP/1.1 over TCP; the
// workspace has no wiremock dep, and `Connection: close` makes
// reqwest open a fresh connection per request).
// ---------------------------------------------------------------------------

struct Fixture {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    _task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
                        .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
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
                    let (status, reason, body) = route(&path);
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{body}",
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

fn route(path: &str) -> (u16, &'static str, String) {
    let json = |v: serde_json::Value| v.to_string();
    if path == "/members" {
        return (
            200,
            "OK",
            json(serde_json::json!([
                {
                    "id": "12345678-9012-3456-7890-123456789012",
                    "profile": {
                        "name": "Jane Doe",
                        "mention_name": "jane",
                        "email_address": "jane@example.com"
                    },
                    "disabled": false
                },
                {
                    "id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                    "profile": {
                        "name": "John Roe",
                        "mention_name": "john",
                        "email_address": "john@example.com"
                    },
                    "disabled": false
                }
            ])),
        );
    }
    if path.starts_with("/search/stories") {
        return (
            200,
            "OK",
            json(serde_json::json!({
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
            json(serde_json::json!([
                {"id": "c1", "author_id": "u1", "text": "looks **good**", "created_at": "2026-11-20T10:00:00Z"}
            ])),
        );
    }
    if path.starts_with("/stories/101") {
        return (
            200,
            "OK",
            json(serde_json::json!({
                "id": 101,
                "name": "Add dark mode",
                "story_type": "feature",
                "workflow_state_id": 500000001,
                "labels": [{"id": 9, "name": "ui", "color": "#8ab4ff"}],
                "description": "As a user I want **dark mode**\n\n- [ ] a\n- [ ] b",
                "app_url": "https://app.shortcut.com/ws/story/101"
            })),
        );
    }
    if path == "/bad-token" {
        return (
            401,
            "Unauthorized",
            json(serde_json::json!({"message": "Invalid API token"})),
        );
    }
    (
        404,
        "Not Found",
        json(serde_json::json!({"message": "Not Found"})),
    )
}

// ---------------------------------------------------------------------------
// Registry / ServiceDef shape tests
// ---------------------------------------------------------------------------

#[test]
fn agent_registry_contains_exactly_ten_shortcut_tools() {
    use nagent_agents::config::AgentConfigs;
    use nagent_agents::egress::EgressPool;
    use nagent_agents::AgentRegistry;
    let registry = AgentRegistry::from_config(&AgentConfigs::default(), true, &EgressPool::new());
    let names: Vec<String> = registry.iter().map(|a| a.name().to_string()).collect();
    let shortcut: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("shortcut_"))
        .collect();
    assert_eq!(shortcut.len(), 10, "expected exactly ten shortcut_* tools");
    for tool in SHORTCUT_TOOLS {
        assert!(
            names.iter().any(|n| n == tool),
            "missing tool `{tool}` in registry"
        );
    }
    // Write tools are deliberately out of scope for v1.
    for forbidden in [
        "shortcut_create_story",
        "shortcut_comment",
        "shortcut_move_story",
    ] {
        assert!(
            !names.iter().any(|n| n == forbidden),
            "`{forbidden}` must NOT be registered in v1 (read-only)"
        );
    }
}

#[test]
fn shortcut_service_def_shape() {
    use nagent_agents::shortcut_service::SHORTCUT_SERVICE;
    use nagent_agents::{FieldKind, ServiceRegistry};
    let reg = ServiceRegistry::new(&[SHORTCUT_SERVICE]);
    let svc = reg.get("shortcut").expect("shortcut service registered");
    assert_eq!(svc.fields.len(), 3);
    let token = &svc.fields[0];
    assert_eq!(token.key, "api_token");
    assert_eq!(token.kind, FieldKind::Password);
    assert!(token.required);
    assert!(
        !token.echo_on_edit,
        "API token must never be echoed on edit"
    );
    let username = &svc.fields[1];
    assert_eq!(username.key, "username");
    assert_eq!(username.kind, FieldKind::Text);
    assert!(username.required);
    assert!(!username.echo_on_edit, "username is credential-adjacent");
    // The stories-dashboard toggle: a checkbox, optional, echoed
    // back on edit (not a secret — wire format "true"/"false").
    let dashboard = &svc.fields[2];
    assert_eq!(dashboard.key, "show_dashboard");
    assert_eq!(dashboard.kind, FieldKind::Checkbox);
    assert!(!dashboard.required, "the dashboard toggle is optional");
    assert!(
        dashboard.echo_on_edit,
        "the checkbox state must be echoed so the form stays ticked on reopen"
    );
}

#[test]
fn descriptor_build_matches_ids() {
    // Mirrors the agents-crate invariant at the server boundary:
    // every shortcut descriptor builds an agent with a matching name.
    use nagent_agents::config::AgentConfigs;
    use nagent_agents::egress::EgressPool;
    use nagent_agents::AgentRegistry;
    let registry = AgentRegistry::from_config(&AgentConfigs::default(), true, &EgressPool::new());
    for tool in SHORTCUT_TOOLS {
        let agent = registry
            .get(tool)
            .unwrap_or_else(|| panic!("`{tool}` not in registry"));
        assert_eq!(agent.name(), *tool);
        assert!(
            agent.untrusted_output(),
            "shortcut tools return remote content → untrusted_output must be true"
        );
    }
}

// ---------------------------------------------------------------------------
// Full HTTP flow against the loopback fixture
// ---------------------------------------------------------------------------

/// Boot a test app wired with auth + the shortcut ServiceDef + an
/// agent registry pointed at the loopback fixture, then drive the
/// PUT → probe → invoke → list flow over HTTP.
#[tokio::test]
async fn put_probe_invoke_list_flow_against_loopback() {
    use nagent_agents::shortcut_service::SHORTCUT_SERVICE;
    use nagent_agents::ServiceRegistry;
    use nagent_server::credentials::{CredentialResolver, CredentialsKey};

    let fx = Fixture::start().await;
    let auth_store = temp_store().await;
    let key = Arc::new(CredentialsKey::from_bytes([0x2bu8; 32]));
    let resolver = CredentialResolver::new(auth_store.clone(), key.clone(), None, None);

    // Seed a user + session; the test authenticates with
    // `Authorization: Bearer <session token>` (bearer requests
    // skip the CSRF check).
    let user_id = auth_store
        .admin()
        .users
        .create("jane@example.com", "Jane", "local", Some(b"hash"))
        .await
        .expect("create user");
    let session = auth_store
        .admin()
        .sessions
        .create(user_id, Duration::from_secs(3600), None, None)
        .await
        .expect("create session");
    let bearer = format!(
        "Bearer {}",
        session.plaintext_token.clone().expect("plaintext token")
    );

    // Agent registry pointed at the loopback fixture.
    let mut cfgs = nagent_agents::config::AgentConfigs::default();
    cfgs.shortcut.base_url = fx.base_url();
    cfgs.shortcut.allowlist = vec!["127.0.0.1".to_string()];
    let registry = nagent_agents::AgentRegistry::from_config(
        &cfgs,
        true,
        &nagent_agents::egress::EgressPool::new(),
    );

    let mut builder = nagent_server::testing::app_state();
    Arc::make_mut(&mut builder.config).auth.enabled = true;
    Arc::make_mut(&mut builder.config).auth.backends =
        vec![nagent_server::config::AuthBackendKind::Local];
    Arc::make_mut(&mut builder.config).auth.public_url = "https://example.com".into();
    Arc::make_mut(&mut builder.config).auth.db = nagent_server::config::AuthDbConfig {
        backend: "sqlite".into(),
        url: format!(
            "sqlite://file:shortcut_e2e_http_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        ),
        max_connections: 1,
        auto_migrate: true,
    };
    // The probe reads `state.config.agents.shortcut`.
    Arc::make_mut(&mut builder.config).agents.shortcut.base_url = fx.base_url();
    Arc::make_mut(&mut builder.config).agents.shortcut.allowlist = vec!["127.0.0.1".to_string()];
    let services = Arc::new(ServiceRegistry::new(&[SHORTCUT_SERVICE]));
    let state = builder
        .with_auth(auth_store.clone())
        .with_credential_resolver(Arc::new(resolver), key)
        .with_services(services)
        .with_agents(registry)
        .build();

    let auth_state = state.auth.as_ref().expect("auth wired").clone();
    let auth_layer = axum::middleware::from_fn_with_state(
        auth_state,
        nagent_server::auth::middleware::require_auth_middleware,
    );
    let identity = nagent_server::auth::router::build_protected_auth_router(state.clone());
    let cred_routes =
        nagent_server::credentials::routes::build_protected_credentials_router(state.clone());
    let probe_routes =
        nagent_server::credentials::shortcut_probe::build_shortcut_probe_router(state.clone());
    let agents_routes: axum::Router<Arc<nagent_server::AppState>> = axum::Router::new()
        .route(
            "/v1/agents",
            axum::routing::get(nagent_server::agents::routes::agents_list),
        )
        .route(
            "/v1/agents/:name/invoke",
            axum::routing::post(nagent_server::agents::routes::agent_invoke),
        );
    let app = identity
        .merge(cred_routes)
        .merge(probe_routes)
        .merge(agents_routes)
        .layer(auth_layer)
        .with_state(state.clone());

    // 1. PUT the API token + username into the vault through the
    //    `shortcut` ServiceDef flow.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/api/integrations/shortcut/credentials")
                .header(axum::http::header::AUTHORIZATION, &bearer)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "fields": {
                            "api_token": TEST_TOKEN,
                            "username": "jane"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "PUT credentials");

    // 2. Probe: POST /api/integrations/shortcut/probe → GET
    //    /members on the fixture → member matched by `username` →
    //    {mention_name: "jane", …}.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/api/integrations/shortcut/probe")
                .header(axum::http::header::AUTHORIZATION, &bearer)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"api_token": TEST_TOKEN, "username": "jane"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "probe must succeed");
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let probe_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(probe_json["ok"], true);
    assert_eq!(probe_json["mention_name"], "jane");
    assert_eq!(probe_json["name"], "Jane Doe");
    let probe_text = String::from_utf8_lossy(&body_bytes);
    assert!(
        !probe_text.contains(TEST_TOKEN),
        "AGENTS.md §6: probe response must not contain the token; got {probe_text}"
    );

    // 2b. Probe with an unknown mention name → 502, token redacted.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/api/integrations/shortcut/probe")
                .header(axum::http::header::AUTHORIZATION, &bearer)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"api_token": TEST_TOKEN, "username": "nobody"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "unknown handle must fail with 502"
    );
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(
        text.contains("nobody"),
        "error should name the handle: {text}"
    );
    assert!(
        !text.contains(TEST_TOKEN),
        "AGENTS.md §6: probe error must not contain the token; got {text}"
    );

    // 3. Direct invoke: POST /v1/agents/shortcut_list_backlog/invoke.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/v1/agents/shortcut_list_backlog/invoke")
                .header(axum::http::header::AUTHORIZATION, &bearer)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"arguments": {"limit": 10}}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "invoke must succeed");
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let invoke_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(invoke_json["name"], "shortcut_list_backlog");
    let result: serde_json::Value =
        serde_json::from_str(invoke_json["result"].as_str().expect("result string")).unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["data"]["count"], 1);
    assert_eq!(result["data"]["stories"][0]["id"], 101);
    assert_eq!(
        result["data"]["query"], "is:story is:backlog !is:archived",
        "the default backlog query must be reported"
    );
    let invoke_text = String::from_utf8_lossy(&body_bytes);
    assert!(
        !invoke_text.contains(TEST_TOKEN),
        "AGENTS.md §6: invoke response must not contain the token"
    );
    // The fixture saw the Shortcut-Token header on both upstream calls.
    for (_, token) in fx.requests() {
        assert_eq!(token, TEST_TOKEN);
    }
    assert!(
        fx.requests().len() >= 2,
        "probe + invoke both hit the fixture"
    );
    assert!(
        fx.requests()
            .iter()
            .any(|(line, _)| line.starts_with("GET /members ")),
        "probe must call GET /members (the v3 API has no /members/me); got {:?}",
        fx.requests()
    );
    assert!(
        !fx.requests()
            .iter()
            .any(|(line, _)| line.contains("/members/me")),
        "no request may hit the non-existent /members/me route; got {:?}",
        fx.requests()
    );
    // The backlog invoke hit the search endpoint with the default query.
    assert!(
        fx.requests()
            .iter()
            .any(|(line, _)| line.starts_with("GET /search/stories?")
                && line.contains("query=is%3Astory+is%3Abacklog+%21is%3Aarchived")),
        "list_backlog must call /search/stories with the configured query; got {:?}",
        fx.requests()
    );

    // 4. GET /v1/agents lists the ten shortcut tools.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .uri("/v1/agents")
                .header(axum::http::header::AUTHORIZATION, &bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    let names: Vec<&str> = list_json["data"]
        .as_array()
        .expect("data array")
        .iter()
        .filter_map(|a| a["name"].as_str())
        .filter(|n| n.starts_with("shortcut_"))
        .collect();
    assert_eq!(
        names.len(),
        10,
        "GET /v1/agents must list the ten shortcut tools"
    );
    for tool in SHORTCUT_TOOLS {
        assert!(names.contains(tool), "missing `{tool}` in /v1/agents");
    }
}

/// The probe is fail-closed when the operator allowlist is empty,
/// and never leaks the token in the error body.
#[tokio::test]
async fn probe_is_fail_closed_and_redacted() {
    use nagent_agents::shortcut_service::SHORTCUT_SERVICE;
    use nagent_agents::ServiceRegistry;
    use nagent_server::credentials::{CredentialResolver, CredentialsKey};

    let auth_store = temp_store().await;
    let key = Arc::new(CredentialsKey::from_bytes([0x2bu8; 32]));
    let resolver = CredentialResolver::new(auth_store.clone(), key.clone(), None, None);
    let user_id = auth_store
        .admin()
        .users
        .create("jane@example.com", "Jane", "local", Some(b"hash"))
        .await
        .expect("create user");
    let session = auth_store
        .admin()
        .sessions
        .create(user_id, Duration::from_secs(3600), None, None)
        .await
        .expect("create session");
    let bearer = format!(
        "Bearer {}",
        session.plaintext_token.clone().expect("plaintext token")
    );

    let mut builder = nagent_server::testing::app_state();
    Arc::make_mut(&mut builder.config).auth.enabled = true;
    Arc::make_mut(&mut builder.config).auth.backends =
        vec![nagent_server::config::AuthBackendKind::Local];
    Arc::make_mut(&mut builder.config).auth.public_url = "https://example.com".into();
    Arc::make_mut(&mut builder.config).auth.db = nagent_server::config::AuthDbConfig {
        backend: "sqlite".into(),
        url: format!(
            "sqlite://file:shortcut_e2e_closed_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        ),
        max_connections: 1,
        auto_migrate: true,
    };
    // Empty allowlist → fail-closed. (Default ShortcutConfig has
    // ["api.app.shortcut.com"]; force empty here.)
    Arc::make_mut(&mut builder.config).agents.shortcut.allowlist = Vec::new();
    let services = Arc::new(ServiceRegistry::new(&[SHORTCUT_SERVICE]));
    let state = builder
        .with_auth(auth_store.clone())
        .with_credential_resolver(Arc::new(resolver), key)
        .with_services(services)
        .build();

    let auth_state = state.auth.as_ref().expect("auth wired").clone();
    let auth_layer = axum::middleware::from_fn_with_state(
        auth_state,
        nagent_server::auth::middleware::require_auth_middleware,
    );
    let probe_routes =
        nagent_server::credentials::shortcut_probe::build_shortcut_probe_router(state.clone());
    let app = probe_routes.layer(auth_layer).with_state(state);

    let resp = app
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/api/integrations/shortcut/probe")
                .header(axum::http::header::AUTHORIZATION, &bearer)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"api_token": TEST_TOKEN}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "empty allowlist must fail closed"
    );
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(
        !text.contains(TEST_TOKEN),
        "AGENTS.md §6: even the fail-closed error body must not carry the token; got {text}"
    );
}

// ---------------------------------------------------------------------------
// Pure-API stories dashboard endpoints (GET /api/integrations/shortcut/stories…)
// ---------------------------------------------------------------------------

/// Boot a test app with auth + credentials routes + the shortcut API
/// dashboard routes, all pointed at the loopback fixture, with the
/// token + username + `show_dashboard` flag already in the vault.
/// Returns the app, the bearer header and the app state.
async fn boot_dashboard_app(fx: &Fixture) -> (axum::Router, String, Arc<nagent_server::AppState>) {
    use nagent_agents::shortcut_service::SHORTCUT_SERVICE;
    use nagent_agents::ServiceRegistry;
    use nagent_server::credentials::{CredentialResolver, CredentialsKey};

    let auth_store = temp_store().await;
    let key = Arc::new(CredentialsKey::from_bytes([0x2bu8; 32]));
    let resolver = CredentialResolver::new(auth_store.clone(), key.clone(), None, None);

    let user_id = auth_store
        .admin()
        .users
        .create("jane@example.com", "Jane", "local", Some(b"hash"))
        .await
        .expect("create user");
    let session = auth_store
        .admin()
        .sessions
        .create(user_id, Duration::from_secs(3600), None, None)
        .await
        .expect("create session");
    let bearer = format!(
        "Bearer {}",
        session.plaintext_token.clone().expect("plaintext token")
    );

    let mut builder = nagent_server::testing::app_state();
    Arc::make_mut(&mut builder.config).auth.enabled = true;
    Arc::make_mut(&mut builder.config).auth.backends =
        vec![nagent_server::config::AuthBackendKind::Local];
    Arc::make_mut(&mut builder.config).auth.public_url = "https://example.com".into();
    Arc::make_mut(&mut builder.config).auth.db = nagent_server::config::AuthDbConfig {
        backend: "sqlite".into(),
        url: format!(
            "sqlite://file:shortcut_e2e_dash_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        ),
        max_connections: 1,
        auto_migrate: true,
    };
    Arc::make_mut(&mut builder.config).agents.shortcut.base_url = fx.base_url();
    Arc::make_mut(&mut builder.config).agents.shortcut.allowlist = vec!["127.0.0.1".to_string()];
    let services = Arc::new(ServiceRegistry::new(&[SHORTCUT_SERVICE]));
    let state = builder
        .with_auth(auth_store.clone())
        .with_credential_resolver(Arc::new(resolver), key.clone())
        .with_services(services)
        .build();

    let auth_state = state.auth.as_ref().expect("auth wired").clone();
    let auth_layer = axum::middleware::from_fn_with_state(
        auth_state,
        nagent_server::auth::middleware::require_auth_middleware,
    );
    let cred_routes =
        nagent_server::credentials::routes::build_protected_credentials_router(state.clone());
    let api_routes =
        nagent_server::credentials::shortcut_api::build_shortcut_api_router(state.clone());
    let app = cred_routes
        .merge(api_routes)
        .layer(auth_layer)
        .with_state(state.clone());

    // Seed the vault: token + username + the dashboard checkbox ON.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/api/integrations/shortcut/credentials")
                .header(axum::http::header::AUTHORIZATION, &bearer)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "fields": {
                            "api_token": TEST_TOKEN,
                            "username": "jane",
                            "show_dashboard": "true"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "PUT credentials");

    (app, bearer, state)
}

/// The dashboard endpoints read the vault token and proxy the
/// Shortcut API directly (never the LLM): list with the default
/// `owner:<mention_name>` query, list with an explicit query,
/// story detail with full description + comments, 404 on unknown
/// id — and no response ever carries the token (AGENTS.md §6).
/// Plain GET helper for the dashboard test: bearer-authenticated,
/// empty body, returns the raw response.
async fn get(app: axum::Router, bearer: &str, uri: &str) -> axum::response::Response {
    app.oneshot(
        HttpRequest::builder()
            .method("GET")
            .uri(uri)
            .header(axum::http::header::AUTHORIZATION, bearer)
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn stories_dashboard_endpoints_against_loopback() {
    let fx = Fixture::start().await;
    let (app, bearer, _state) = boot_dashboard_app(&fx).await;
    macro_rules! get {
        ($uri:expr) => {
            get(app.clone(), &bearer, $uri).await
        };
    }

    // 0. The `show_dashboard` checkbox round-trips through
    //    GET /api/integrations/shortcut (echo_on_edit path).
    let resp = get!("/api/integrations/shortcut");
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let svc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(svc["configured"], true);
    let dash = svc["fields"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["key"] == "show_dashboard")
        .expect("show_dashboard field present");
    assert_eq!(dash["kind"], "checkbox");
    assert_eq!(dash["value"], "true", "checkbox state must be echoed");
    assert_eq!(dash["filled"], true);
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(
        !text.contains(TEST_TOKEN),
        "GET service must not leak the token"
    );

    // 0b. `GET /api/integrations/shortcut/me` resolves the vault
    //     token's member and returns the display identity (mention
    //     name) — never the token.
    let resp = get(app.clone(), &bearer, "/api/integrations/shortcut/me").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let me: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(me["data"]["mention_name"], "jane");
    assert_eq!(me["data"]["name"], "Jane Doe");
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(
        !text.contains(TEST_TOKEN),
        "AGENTS.md §6: /me response must not contain the token; got {text}"
    );

    // 0c. A partial PUT (only the checkbox) must KEEP the saved token
    //     and username — the password field is never echoed to the
    //     browser, so it is absent from the body on edit and must be
    //     carried over server-side, never wiped.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/api/integrations/shortcut/credentials")
                .header(axum::http::header::AUTHORIZATION, &bearer)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({ "fields": { "show_dashboard": "false" } }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "partial PUT");
    let resp = get(app.clone(), &bearer, "/api/integrations/shortcut").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let svc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    let by_key = |k: &str| {
        svc["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == k)
            .unwrap()
            .clone()
    };
    assert_eq!(
        by_key("api_token")["filled"],
        true,
        "token must survive a partial PUT"
    );
    assert_eq!(
        by_key("username")["filled"],
        true,
        "username must survive a partial PUT"
    );
    assert_eq!(
        by_key("show_dashboard")["value"],
        "false",
        "checkbox was updated"
    );
    // …and the dashboard still works with the preserved token.
    let resp = get(app.clone(), &bearer, "/api/integrations/shortcut/stories").await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "list must still work after partial PUT"
    );
    // An empty-fields PUT is a documented no-op: nothing wiped.
    let resp = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("PUT")
                .uri("/api/integrations/shortcut/credentials")
                .header(axum::http::header::AUTHORIZATION, &bearer)
                .header(axum::http::header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::json!({ "fields": {} }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = get(app.clone(), &bearer, "/api/integrations/shortcut").await;
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let svc: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(
        svc["fields"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["filled"] == true),
        "empty PUT must not wipe any field: {svc}"
    );

    // 1. Default list: the server composes `owner:jane !is:archived
    //    !is:done` — scoped to the caller, done stories excluded by
    //    default.
    let resp = get!("/api/integrations/shortcut/stories");
    assert_eq!(resp.status(), StatusCode::OK, "list must succeed");
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(list["data"]["count"], 1);
    assert_eq!(list["data"]["stories"][0]["id"], 101);
    assert_eq!(list["data"]["stories"][0]["name"], "Add dark mode");
    assert_eq!(
        list["data"]["stories"][0]["app_url"], "https://app.shortcut.com/ws/story/101",
        "list rows must carry app_url so the UI can render the external link"
    );
    assert_eq!(list["data"]["query"], "owner:jane !is:archived !is:done");
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(
        !text.contains(TEST_TOKEN),
        "AGENTS.md §6: list response must not contain the token; got {text}"
    );
    assert!(
        fx.requests()
            .iter()
            .any(|(line, token)| line.starts_with("GET /search/stories?")
                && line.contains("query=owner%3Ajane+%21is%3Aarchived+%21is%3Adone")
                && line.contains("page_size=30")
                && token == TEST_TOKEN),
        "default list must scope to owner:jane and exclude done; got {:?}",
        fx.requests()
    );

    // 2. A free-text filter is ANDed with the owner scope — it can
    //    never list other people's stories.
    let resp = get!("/api/integrations/shortcut/stories?query=is%3Astory+is%3Astarted&limit=5");
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        list["data"]["query"],
        "owner:jane !is:archived !is:done is:story is:started"
    );
    assert!(
        fx.requests().iter().any(|(line, _)| line
            .contains("query=owner%3Ajane+%21is%3Aarchived+%21is%3Adone+is%3Astory+is%3Astarted")
            && line.contains("page_size=5")),
        "user filter must be ANDed with owner:jane; got {:?}",
        fx.requests()
    );

    // 2b. `include_done=true` drops the `!is:done` term.
    let resp = get!("/api/integrations/shortcut/stories?include_done=true");
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(list["data"]["query"], "owner:jane !is:archived");
    assert!(
        fx.requests().iter().any(
            |(line, _)| line.contains("query=owner%3Ajane+%21is%3Aarchived")
                && !line.contains("is%3Adone")
        ),
        "include_done=true must omit !is:done; got {:?}",
        fx.requests()
    );

    // 2c. A query that already pins `owner:` is respected (the owner
    //     scope is not doubled).
    let resp = get!("/api/integrations/shortcut/stories?query=owner%3Ajohn+is%3Astarted");
    assert_eq!(resp.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        list["data"]["query"],
        "!is:archived !is:done owner:john is:started"
    );

    // 3. Story detail: full description + comments, two upstream calls.
    let resp = get!("/api/integrations/shortcut/stories/101");
    assert_eq!(resp.status(), StatusCode::OK, "detail must succeed");
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let detail: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(detail["data"]["story"]["id"], 101);
    assert_eq!(
        detail["data"]["story"]["app_url"], "https://app.shortcut.com/ws/story/101",
        "detail must carry app_url for the overlay's external link"
    );
    assert_eq!(
        detail["data"]["story"]["description"],
        "As a user I want **dark mode**\n\n- [ ] a\n- [ ] b",
        "the dashboard gets the FULL description (no LLM cap)"
    );
    assert_eq!(detail["data"]["story"]["labels"][0]["name"], "ui");
    assert_eq!(detail["data"]["story"]["labels"][0]["color"], "#8ab4ff");
    assert_eq!(detail["data"]["comments"][0]["text"], "looks **good**");
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(
        !text.contains(TEST_TOKEN),
        "AGENTS.md §6: detail response must not contain the token; got {text}"
    );
    assert!(
        fx.requests()
            .iter()
            .any(|(line, _)| line.starts_with("GET /stories/101/comments")),
        "detail must fetch the comments; got {:?}",
        fx.requests()
    );

    // 4. Unknown story → 404 with a JSON error, no token.
    let resp = get!("/api/integrations/shortcut/stories/999999");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(text.contains("error"), "404 body must carry an error field");
    assert!(!text.contains(TEST_TOKEN));
}

/// Without a vault token the dashboard endpoints answer 400 with a
/// "not configured" hint — and never echo a token.
#[tokio::test]
async fn stories_dashboard_requires_configured_token() {
    let fx = Fixture::start().await;
    let auth_store = temp_store().await;
    let key = Arc::new(nagent_server::credentials::CredentialsKey::from_bytes(
        [0x2bu8; 32],
    ));
    let resolver = nagent_server::credentials::CredentialResolver::new(
        auth_store.clone(),
        key.clone(),
        None,
        None,
    );
    let user_id = auth_store
        .admin()
        .users
        .create("jane@example.com", "Jane", "local", Some(b"hash"))
        .await
        .expect("create user");
    let session = auth_store
        .admin()
        .sessions
        .create(user_id, Duration::from_secs(3600), None, None)
        .await
        .expect("create session");
    let bearer = format!(
        "Bearer {}",
        session.plaintext_token.clone().expect("plaintext token")
    );

    let mut builder = nagent_server::testing::app_state();
    Arc::make_mut(&mut builder.config).auth.enabled = true;
    Arc::make_mut(&mut builder.config).auth.backends =
        vec![nagent_server::config::AuthBackendKind::Local];
    Arc::make_mut(&mut builder.config).auth.public_url = "https://example.com".into();
    Arc::make_mut(&mut builder.config).auth.db = nagent_server::config::AuthDbConfig {
        backend: "sqlite".into(),
        url: format!(
            "sqlite://file:shortcut_e2e_unconf_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        ),
        max_connections: 1,
        auto_migrate: true,
    };
    Arc::make_mut(&mut builder.config).agents.shortcut.base_url = fx.base_url();
    Arc::make_mut(&mut builder.config).agents.shortcut.allowlist = vec!["127.0.0.1".to_string()];
    let state = builder
        .with_auth(auth_store.clone())
        .with_credential_resolver(Arc::new(resolver), key)
        .build();
    let auth_state = state.auth.as_ref().expect("auth wired").clone();
    let auth_layer = axum::middleware::from_fn_with_state(
        auth_state,
        nagent_server::auth::middleware::require_auth_middleware,
    );
    let api_routes =
        nagent_server::credentials::shortcut_api::build_shortcut_api_router(state.clone());
    let app = api_routes.layer(auth_layer).with_state(state);

    let resp = app
        .oneshot(
            HttpRequest::builder()
                .method("GET")
                .uri("/api/integrations/shortcut/stories")
                .header(axum::http::header::AUTHORIZATION, &bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body_bytes);
    assert!(
        text.contains("not configured"),
        "hint must point at Settings; got {text}"
    );
    assert!(!text.contains(TEST_TOKEN));
    assert!(
        fx.requests().is_empty(),
        "no upstream call may happen without a vault token"
    );
}
