//! End-to-end tests for the GitHub multi-account support
//! (plan 1795793328177): the dedicated
//! `GET/POST/DELETE /api/integrations/github/accounts` endpoints,
//! the cross-account `?account=` aggregation on the PR dashboard,
//! and the `probe-repo` repo→account recording.
//!
//! The loopback fixture answers **per `Authorization` header**, so
//! the tests can prove that fan-out hits each account with its own
//! token.
//!
//! AGENTS.md §6: every test asserts that no fixture token appears in
//! any HTTP response body (accounts GET, error bodies, audit-facing
//! responses).

#![cfg(feature = "github-agent")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use tower::ServiceExt;
use uuid::Uuid;

const DEFAULT_TOKEN: &str = "ghp_default_token_0000000000000001";
const CLIENT_TOKEN: &str = "ghp_client_token_0000000000000002";
const BAD_TOKEN: &str = "ghp_bad_token_00000000000000000003";
const ALL_TOKENS: &[&str] = &[DEFAULT_TOKEN, CLIENT_TOKEN, BAD_TOKEN];

// ---------------------------------------------------------------------------
// Loopback fixture — routes per `Authorization: Bearer <token>`
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
                    let auth = text
                        .lines()
                        .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                        .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
                        .unwrap_or_default();
                    req_clone
                        .lock()
                        .unwrap()
                        .push((request_line.clone(), auth.clone()));
                    let target = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    let (status, reason, body, extra_headers) = route(&target, &auth);
                    let mut resp = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n",
                        body.len()
                    );
                    for (k, v) in extra_headers {
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

fn is_default(auth: &str) -> bool {
    auth == format!("Bearer {DEFAULT_TOKEN}")
}

fn is_client(auth: &str) -> bool {
    auth == format!("Bearer {CLIENT_TOKEN}")
}

fn pr(number: u64, title: &str, updated_at: &str) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": title,
        "state": "open",
        "draft": false,
        "merged": false,
        "user": {"login": "octo"},
        "labels": [],
        "head": {"ref": format!("fix/{number}"), "sha": format!("sha{number}")},
        "base": {"ref": "main"},
        "created_at": "2026-11-20T10:00:00Z",
        "updated_at": updated_at,
        "html_url": format!("https://github.com/x/y/pull/{number}"),
    })
}

/// Route table keyed on the path AND the bearer token, so tests can
/// assert per-account fan-out. `extra_headers` carries
/// `x-oauth-scopes` on `/user`.
fn route(target: &str, auth: &str) -> (u16, &'static str, String, Vec<(String, String)>) {
    let path = target.split('?').next().unwrap_or(target);
    let json = |v: serde_json::Value| v.to_string();
    let ok = |v: serde_json::Value| (200, "OK", json(v), Vec::new());
    let not_found = || {
        (
            404,
            "Not Found",
            json(serde_json::json!({"message": "Not Found"})),
            Vec::new(),
        )
    };
    match path {
        "/user" => {
            if is_default(auth) {
                (
                    200,
                    "OK",
                    json(serde_json::json!({"login": "octo", "plan": {"name": "pro"}})),
                    vec![("x-oauth-scopes".to_string(), "repo, read:user".to_string())],
                )
            } else if is_client(auth) {
                (
                    200,
                    "OK",
                    json(serde_json::json!({"login": "clientbot", "plan": {"name": "free"}})),
                    vec![("x-oauth-scopes".to_string(), "repo".to_string())],
                )
            } else {
                (
                    401,
                    "Unauthorized",
                    json(serde_json::json!({"message": "Bad credentials"})),
                    Vec::new(),
                )
            }
        }
        "/user/repos" => {
            if is_default(auth) {
                ok(serde_json::json!([{
                    "full_name": "octo/hello",
                    "owner": {"login": "octo"},
                    "private": false,
                    "pushed_at": "2026-11-26T00:00:00Z",
                    "html_url": "https://github.com/octo/hello"
                }]))
            } else if is_client(auth) {
                ok(serde_json::json!([{
                    "full_name": "client/api",
                    "owner": {"login": "client"},
                    "private": true,
                    "pushed_at": "2026-11-25T00:00:00Z",
                    "html_url": "https://github.com/client/api"
                }]))
            } else {
                not_found()
            }
        }
        "/user/orgs" => {
            if is_default(auth) {
                ok(serde_json::json!([{"login": "acme"}]))
            } else if is_client(auth) {
                ok(serde_json::json!([{"login": "clientorg"}]))
            } else {
                not_found()
            }
        }
        "/orgs/clientorg/repos" => {
            if is_client(auth) {
                ok(serde_json::json!([{
                    "full_name": "client/api",
                    "owner": {"login": "client"},
                    "private": true,
                    "pushed_at": "2026-11-25T00:00:00Z",
                    "html_url": "https://github.com/client/api"
                }]))
            } else {
                not_found()
            }
        }
        "/repos/octo/hello" => {
            if is_default(auth) {
                ok(serde_json::json!({
                    "full_name": "octo/hello",
                    "default_branch": "main",
                    "private": false,
                    "html_url": "https://github.com/octo/hello"
                }))
            } else {
                not_found()
            }
        }
        "/repos/octo/hello/pulls" => {
            if is_default(auth) {
                ok(serde_json::json!([pr(
                    7,
                    "Default PR",
                    "2026-11-24T10:00:00Z"
                )]))
            } else {
                not_found()
            }
        }
        "/repos/client/api" => {
            if is_client(auth) {
                ok(serde_json::json!({
                    "full_name": "client/api",
                    "default_branch": "main",
                    "private": true,
                    "html_url": "https://github.com/client/api"
                }))
            } else {
                // The default token cannot see the client repo — this
                // is what drives repo→account resolution.
                not_found()
            }
        }
        "/repos/client/api/pulls" => {
            if is_client(auth) {
                ok(serde_json::json!([pr(
                    3,
                    "Client PR",
                    "2026-11-26T10:00:00Z"
                )]))
            } else {
                not_found()
            }
        }
        _ => not_found(),
    }
}

// ---------------------------------------------------------------------------
// App boot
// ---------------------------------------------------------------------------

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
                "sqlite://file:github_accounts_e2e_{}?mode=memory&cache=shared",
                Uuid::new_v4()
            ),
            max_connections: 1,
            auto_migrate: true,
        },
        ..Default::default()
    };
    let opts: nagent_db::DbOptions = (&cfg).into();
    let store = nagent_db::Db::connect(&opts).await.expect("store connects");
    store.migrate().await.expect("migrate");
    store
}

struct Harness {
    app: axum::Router,
    bearer: String,
    key: Arc<nagent_server::credentials::CredentialsKey>,
    auth_store: nagent_db::Db,
    user_id: Uuid,
}

/// Boot a test app with auth + the github ServiceDef + accounts / api
/// / probe-repo routers mounted, the loopback fixture as
/// `[agents.github].base_url`, and `max_accounts` configurable.
async fn boot(fx: &Fixture, max_accounts: usize, seed_vault: bool) -> Harness {
    use nagent_agents::github_service::GITHUB_SERVICE;
    use nagent_agents::ServiceRegistry;
    use nagent_server::credentials::{CredentialResolver, CredentialsKey};

    let auth_store = temp_store().await;
    let key = Arc::new(CredentialsKey::from_bytes([0x2bu8; 32]));
    let resolver = CredentialResolver::new(auth_store.clone(), key.clone(), None, None);

    let user_id = auth_store
        .admin()
        .users
        .create("multi@example.com", "Multi", "local", Some(b"hash"))
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
            "sqlite://file:github_accounts_e2e_app_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        ),
        max_connections: 1,
        auto_migrate: true,
    };
    Arc::make_mut(&mut builder.config).agents.github.base_url = fx.base_url();
    Arc::make_mut(&mut builder.config).agents.github.allowlist = vec!["127.0.0.1".to_string()];
    Arc::make_mut(&mut builder.config)
        .agents
        .github
        .max_accounts = max_accounts;
    let services = Arc::new(ServiceRegistry::new(&[GITHUB_SERVICE]));
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
    let accounts_routes =
        nagent_server::credentials::github_accounts::build_github_accounts_router(state.clone());
    let probe_repo_routes =
        nagent_server::credentials::github_probe_repo::build_github_probe_repo_router(
            state.clone(),
        );
    let api_routes = nagent_server::credentials::github_api::build_github_api_router(state.clone());
    let app = cred_routes
        .merge(accounts_routes)
        .merge(probe_repo_routes)
        .merge(api_routes)
        .layer(auth_layer)
        .with_state(state.clone());

    let app = app;
    let h = Harness {
        app,
        bearer,
        key,
        auth_store,
        user_id,
    };

    if seed_vault {
        let resp = h
            .app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("PUT")
                    .uri("/api/integrations/github/credentials")
                    .header(axum::http::header::AUTHORIZATION, &h.bearer)
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "fields": {
                                "personal_access_token": DEFAULT_TOKEN,
                                "username": "octo",
                                "show_dashboard": "true",
                                "dashboard_repos": "[\"octo/hello\",\"client/api\"]"
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT, "PUT credentials");
    }

    h
}

// ---------------------------------------------------------------------------
// HTTP helpers — every response body is captured into `bodies` so the
// §6 assertion can scan all of them at once.
// ---------------------------------------------------------------------------

struct Seen {
    bodies: Vec<String>,
}

impl Seen {
    fn new() -> Self {
        Self { bodies: Vec::new() }
    }

    fn assert_no_tokens(&self) {
        for body in &self.bodies {
            for token in ALL_TOKENS {
                assert!(
                    !body.contains(token),
                    "AGENTS.md §6 violation: response body leaks token {token}: {body}"
                );
            }
        }
    }
}

async fn get(h: &Harness, seen: &mut Seen, uri: &str) -> (StatusCode, serde_json::Value) {
    let resp = h
        .app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("GET")
                .uri(uri)
                .header(axum::http::header::AUTHORIZATION, &h.bearer)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    seen.bodies.push(text.clone());
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn send(
    h: &Harness,
    seen: &mut Seen,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = HttpRequest::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, &h.bearer);
    let body = match body {
        Some(v) => {
            builder = builder.header(axum::http::header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = h
        .app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    seen.bodies.push(text.clone());
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Read + decrypt one vault row straight from the store (test-side
/// inspection of what the endpoints persisted).
async fn vault_plaintext(h: &Harness, field: &str) -> Option<String> {
    use secrecy::ExposeSecret;
    let row = h
        .auth_store
        .for_user(h.user_id)
        .credentials()
        .fetch("github", field)
        .await
        .expect("vault fetch")
        .expect("row present");
    let plain = nagent_server::credentials::crypto::decrypt(
        &h.key,
        &nagent_server::credentials::crypto::EncryptedSecret {
            nonce: row.nonce,
            ciphertext: row.ciphertext,
        },
    )
    .expect("decrypt");
    Some(plain.expose_secret().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accounts_crud_masking_and_refusals() {
    let fx = Fixture::start().await;
    let h = boot(&fx, 8, true).await;
    let mut seen = Seen::new();

    // GET with only the default PAT: one "default" account, token-free.
    let (status, body) = get(&h, &mut seen, "/api/integrations/github/accounts").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["default_account"], "default");
    let accounts = body["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["name"], "default");
    assert_eq!(accounts[0]["username"], "octo");
    assert_eq!(accounts[0]["is_default"], true);
    assert!(
        accounts[0].get("token").is_none(),
        "the accounts list must never carry a token (AGENTS.md §6)"
    );

    // Add a named account — the fixture answers /user per token.
    let (status, body) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "client-a", "token": CLIENT_TOKEN})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "add account: {body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["updated"], false);
    assert_eq!(body["account"]["name"], "client-a");
    assert_eq!(body["account"]["username"], "clientbot");
    assert_eq!(body["account"]["scopes"], "repo");

    // GET now lists both; still token-free.
    let (status, body) = get(&h, &mut seen, "/api/integrations/github/accounts").await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["default", "client-a"]);

    // Re-POST the same name → update.
    let (status, body) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "client-a", "token": CLIENT_TOKEN})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["updated"], true);

    // Invalid names / empty token / bad token.
    let (status, _) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "Bad Name", "token": CLIENT_TOKEN})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "default", "token": CLIENT_TOKEN})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "\"default\" is reserved");
    let (status, _) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "empty", "token": "  "})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, body) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "bad", "token": BAD_TOKEN})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "fixture 401s the bad token"
    );
    // The wire client's 401 message is a fixed reconnect hint — the
    // token itself must not appear (asserted globally below too).
    assert!(
        body["error"].as_str().unwrap().contains("401"),
        "401 hint surfaces: {body}"
    );

    // DELETE "default" is refused.
    let (status, _) = send(
        &h,
        &mut seen,
        "DELETE",
        "/api/integrations/github/accounts/default",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Point default_account at client-a, then refuse to delete it.
    let (status, _) = send(
        &h,
        &mut seen,
        "PUT",
        "/api/integrations/github/credentials",
        Some(serde_json::json!({"fields": {"default_account": "client-a"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = get(&h, &mut seen, "/api/integrations/github/accounts").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["default_account"], "client-a");
    let (status, _) = send(
        &h,
        &mut seen,
        "DELETE",
        "/api/integrations/github/accounts/client-a",
        None,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "cannot delete the default account"
    );

    // Repoint to default, then delete succeeds.
    let (status, _) = send(
        &h,
        &mut seen,
        "PUT",
        "/api/integrations/github/credentials",
        Some(serde_json::json!({"fields": {"default_account": "default"}})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(
        &h,
        &mut seen,
        "DELETE",
        "/api/integrations/github/accounts/client-a",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = get(&h, &mut seen, "/api/integrations/github/accounts").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["accounts"].as_array().unwrap().len(), 1);

    // Deleting an unknown name → 404.
    let (status, _) = send(
        &h,
        &mut seen,
        "DELETE",
        "/api/integrations/github/accounts/ghost",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    seen.assert_no_tokens();
}

#[tokio::test]
async fn accounts_cap_is_enforced() {
    let fx = Fixture::start().await;
    // max_accounts = 2 → two extra accounts fit, the third is refused.
    let h = boot(&fx, 2, true).await;
    let mut seen = Seen::new();

    for name in ["client-a", "client-b"] {
        let (status, body) = send(
            &h,
            &mut seen,
            "POST",
            "/api/integrations/github/accounts",
            Some(serde_json::json!({"name": name, "token": CLIENT_TOKEN})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}: {body}");
    }
    let (status, body) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "client-c", "token": CLIENT_TOKEN})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("max_accounts"),
        "cap error names the limit, not the blob: {body}"
    );
    // Updating an existing name still works at the cap.
    let (status, body) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "client-a", "token": CLIENT_TOKEN})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "update at cap: {body}");
    assert_eq!(body["updated"], true);

    seen.assert_no_tokens();
}

#[tokio::test]
async fn prs_dashboard_aggregates_across_accounts_with_account_filter() {
    let fx = Fixture::start().await;
    let h = boot(&fx, 8, true).await;
    let mut seen = Seen::new();

    // Add the client account through the dedicated endpoint.
    let (status, body) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "client-a", "token": CLIENT_TOKEN})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Default (?account=all): both repos resolve, rows tagged.
    let (status, body) = get(&h, &mut seen, "/api/integrations/github/prs").await;
    assert_eq!(status, StatusCode::OK, "{}", body);
    assert_eq!(body["data"]["account"], "all");
    let prs = body["data"]["prs"].as_array().unwrap();
    assert_eq!(prs.len(), 2, "both repos contribute one PR each: {body}");
    let by_repo: std::collections::HashMap<&str, &str> = prs
        .iter()
        .map(|p| (p["repo"].as_str().unwrap(), p["account"].as_str().unwrap()))
        .collect();
    assert_eq!(by_repo.get("octo/hello"), Some(&"default"));
    assert_eq!(by_repo.get("client/api"), Some(&"client-a"));
    // Merged by updated_at desc: client PR (11-26) before default (11-24).
    assert_eq!(prs[0]["repo"], "client/api");
    assert_eq!(prs[1]["repo"], "octo/hello");

    // The fan-out really used per-account tokens: the fixture saw the
    // client token on /repos/client/api and the default token on
    // /repos/octo/hello.
    let reqs = fx.requests();
    assert!(
        reqs.iter()
            .any(|(line, auth)| line.starts_with("GET /repos/client/api ")
                && auth == &format!("Bearer {CLIENT_TOKEN}")),
        "client repo must be fetched with the client token: {reqs:?}"
    );
    assert!(
        reqs.iter()
            .any(|(line, auth)| line.starts_with("GET /repos/octo/hello ")
                && auth == &format!("Bearer {DEFAULT_TOKEN}")),
        "default repo must be fetched with the default token: {reqs:?}"
    );

    // ?account=client-a narrows to the client account.
    let (status, body) = get(
        &h,
        &mut seen,
        "/api/integrations/github/prs?account=client-a",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["account"], "client-a");
    let prs = body["data"]["prs"].as_array().unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0]["repo"], "client/api");
    assert_eq!(prs[0]["account"], "client-a");

    // ?account=default narrows to the default account.
    let (status, body) = get(
        &h,
        &mut seen,
        "/api/integrations/github/prs?account=default",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let prs = body["data"]["prs"].as_array().unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0]["repo"], "octo/hello");
    assert_eq!(prs[0]["account"], "default");

    // Unknown account → 400 naming the known accounts, never tokens.
    let (status, body) = get(&h, &mut seen, "/api/integrations/github/prs?account=nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("nope") && err.contains("client-a") && err.contains("default"));

    // Owners endpoints take ?account= too.
    let (status, body) = get(&h, &mut seen, "/api/integrations/github/owners").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = get(&h, &mut seen, "/api/integrations/github/owners?account=all").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let empty: Vec<serde_json::Value> = Vec::new();
    let owners: Vec<&str> = body["data"]["owners"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|o| o["login"].as_str())
        .collect();
    assert!(
        owners.contains(&"acme") && owners.contains(&"clientorg"),
        "{body}"
    );

    seen.assert_no_tokens();
}

#[tokio::test]
async fn probe_repo_records_the_owning_account() {
    let fx = Fixture::start().await;
    let h = boot(&fx, 8, true).await;
    let mut seen = Seen::new();

    let (status, body) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "client-a", "token": CLIENT_TOKEN})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The client repo is invisible to the default token: the probe
    // must fan out and record client-a as the owner.
    let (status, body) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/probe-repo",
        Some(serde_json::json!({"repo": "client/api"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);
    assert_eq!(body["account"], "client-a");
    assert_eq!(body["repo"]["full_name"], "client/api");

    // The mapping is persisted into the `github.repo_accounts` blob.
    let blob = vault_plaintext(&h, "repo_accounts")
        .await
        .expect("repo_accounts row");
    let map: serde_json::Value = serde_json::from_str(&blob).unwrap();
    assert_eq!(map["client/api"], "client-a", "persisted map: {blob}");

    // The default repo resolves to the default account.
    let (status, body) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/probe-repo",
        Some(serde_json::json!({"repo": "octo/hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["account"], "default");

    // A repo no account can read → 404 listing account names only.
    let (status, body) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/probe-repo",
        Some(serde_json::json!({"repo": "octo/secret"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let err = body["error"].as_str().unwrap();
    assert!(err.contains("no access") || err.contains("no configured"));
    assert!(err.contains("default") && err.contains("client-a"));

    seen.assert_no_tokens();
}

/// The `github.accounts` blob itself is stored encrypted and its
/// plaintext is only ever materialised server-side — this test pins
/// that the vault row is ciphertext, not JSON with tokens.
#[tokio::test]
async fn accounts_blob_is_stored_encrypted() {
    let fx = Fixture::start().await;
    let h = boot(&fx, 8, true).await;
    let mut seen = Seen::new();

    let (status, _) = send(
        &h,
        &mut seen,
        "POST",
        "/api/integrations/github/accounts",
        Some(serde_json::json!({"name": "client-a", "token": CLIENT_TOKEN})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let row = h
        .auth_store
        .for_user(h.user_id)
        .credentials()
        .fetch("github", "accounts")
        .await
        .expect("vault fetch")
        .expect("accounts row");
    let raw = String::from_utf8_lossy(&row.ciphertext).into_owned();
    assert!(
        !raw.contains(CLIENT_TOKEN) && !raw.contains("client-a"),
        "the stored blob must be AES-256-GCM ciphertext, not plaintext JSON"
    );
    // And it decrypts to valid JSON with the entry.
    let blob = vault_plaintext(&h, "accounts").await.expect("decrypt");
    let defs = nagent_github::parse_accounts(&blob, 8).expect("parse");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "client-a");

    seen.assert_no_tokens();
}
