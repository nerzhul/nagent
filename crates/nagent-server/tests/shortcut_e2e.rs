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
    assert_eq!(svc.fields.len(), 2);
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
