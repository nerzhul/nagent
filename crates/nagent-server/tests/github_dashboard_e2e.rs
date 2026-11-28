//! End-to-end tests for the GitHub PRs dashboard (plan
//! 1795686864347).
//!
//! Complements `github_e2e.rs` (chat tools + token probe) with the
//! pure-API surface:
//!
//! 1. `POST /api/integrations/github/probe-repo` — access check
//!    with the vault PAT: 200 on an accessible repo, 404 on a
//!    missing/inaccessible one, 400 on a malformed `owner/repo`
//!    and on a missing saved token. Audit rows use the
//!    `github_probe_repo_*` kinds.
//! 2. `GET /api/integrations/github/prs` — aggregation across the
//!    configured `dashboard_repos` sorted by `updated_at` desc,
//!    `?repo=` filter (+400 on an unconfigured repo), `?state=`
//!    validation, and per-repo 404 isolation in `repo_errors`.
//! 3. `GET /api/integrations/github/prs/{owner}/{repo}/{number}` —
//!    detail endpoint gated on the configured repo list.
//! 4. AGENTS.md §6: the PAT never appears in any response body.
//! 5. The `show_dashboard` / `dashboard_repos` fields round-trip
//!    through `PUT/GET /api/integrations/github/credentials`.

#![cfg(feature = "github-agent")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use tower::ServiceExt;
use uuid::Uuid;

const TEST_TOKEN: &str = "ghp_dash_e2e_token_0123456789abcdef";

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
                "sqlite://file:github_dash_e2e_{}?mode=memory&cache=shared",
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
// Loopback GitHub API fixture
// ---------------------------------------------------------------------------

struct Fixture {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    // When set, /repos/octo/hello/pulls returns one extra PR (#11) —
    // used to prove that ?refresh=true bypasses the response cache.
    extra_pr: Arc<std::sync::atomic::AtomicBool>,
    _task: tokio::task::JoinHandle<()>,
}

impl Fixture {
    fn set_extra_pr(&self, on: bool) {
        self.extra_pr.store(on, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Fixture {
    async fn start() -> Self {
        Self::start_with(false).await
    }

    /// `paged` mode answers `/user/repos?page=1` with a full page of
    /// 100 repos and `?page=2` with a short page of 2, so tests can
    /// verify the client walks pages until a short one.
    async fn start_paged() -> Self {
        Self::start_with(true).await
    }

    async fn start_with(paged: bool) -> Self {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let req_clone = requests.clone();
        let extra_pr = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let extra_clone = extra_pr.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let req_clone = req_clone.clone();
                let paged = paged;
                let extra = extra_clone.load(std::sync::atomic::Ordering::SeqCst);
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
                    req_clone.lock().unwrap().push((request_line.clone(), auth));
                    let target = request_line
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_string();
                    let (status, reason, body) = route(&target, paged, extra);
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
            extra_pr,
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

fn pr(number: u64, title: &str, updated_at: &str, draft: bool) -> serde_json::Value {
    pr_full(number, title, updated_at, draft, false)
}

fn pr_full(
    number: u64,
    title: &str,
    updated_at: &str,
    draft: bool,
    merged: bool,
) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "title": title,
        "state": if merged { "closed" } else { "open" },
        "draft": draft,
        "merged": merged,
        "user": {"login": "octo", "avatar_url": "https://avatars.example/octo"},
        "labels": [{"name": "bug", "color": "d73a4a"}],
        "head": {"ref": format!("fix/{number}"), "sha": format!("sha{number}")},
        "base": {"ref": "main"},
        "created_at": "2026-11-20T10:00:00Z",
        "updated_at": updated_at,
        "merged_at": null,
        "html_url": format!("https://github.com/octo/hello/pull/{number}"),
    })
}

fn repo_json(full_name: &str, owner: &str, private: bool, pushed_at: &str) -> serde_json::Value {
    serde_json::json!({
        "full_name": full_name,
        "owner": {"login": owner},
        "description": "",
        "private": private,
        "archived": false,
        "default_branch": "main",
        "pushed_at": pushed_at,
        "html_url": format!("https://github.com/{full_name}"),
    })
}

/// Route table. The request target includes the query string, so
/// match on the path part only — except `/user/repos` in paged mode,
/// which answers per `page=` query parameter.
fn route(target: &str, paged: bool, extra: bool) -> (u16, &'static str, String) {
    let path = target.split('?').next().unwrap_or(target);
    let json = |v: serde_json::Value| v.to_string();
    match path {
        "/user/repos" if paged => {
            let page = target
                .split('?')
                .nth(1)
                .and_then(|q| {
                    q.split('&').find_map(|kv| {
                        let (k, v) = kv.split_once('=')?;
                        (k == "page").then(|| v.parse::<u32>().unwrap_or(0))
                    })
                })
                .unwrap_or(0);
            let repos: Vec<serde_json::Value> = match page {
                // Full page of 100 own repos.
                1 => (0..100)
                    .map(|i| {
                        repo_json(
                            &format!("octo/repo-{i}"),
                            "octo",
                            false,
                            "2026-11-26T00:00:00Z",
                        )
                    })
                    .collect(),
                // Short page: one outside-collaborator private repo
                // and one org private repo.
                2 => vec![
                    repo_json(
                        "secretcorp/billing",
                        "secretcorp",
                        true,
                        "2026-11-24T00:00:00Z",
                    ),
                    repo_json("acme/api", "acme", true, "2026-11-25T00:00:00Z"),
                ],
                _ => vec![],
            };
            (200, "OK", json(serde_json::json!(repos)))
        }
        "/user" => (
            200,
            "OK",
            json(serde_json::json!({"login": "octo", "plan": {"name": "pro"}})),
        ),
        "/user/orgs" => (
            200,
            "OK",
            json(serde_json::json!([
                {"login": "acme", "avatar_url": "https://avatars.example/acme"}
            ])),
        ),
        // The single "everything the token can read" list: own repo,
        // an org-member private repo, and an outside-collaborator
        // private repo of an org that is NOT in /user/orgs.
        "/user/repos" => (
            200,
            "OK",
            json(serde_json::json!([
                {
                    "full_name": "octo/hello",
                    "owner": {"login": "octo"},
                    "description": "demo",
                    "private": false,
                    "archived": false,
                    "default_branch": "main",
                    "pushed_at": "2026-11-26T00:00:00Z",
                    "html_url": "https://github.com/octo/hello"
                },
                {
                    "full_name": "acme/api",
                    "owner": {"login": "acme"},
                    "description": "the api",
                    "private": true,
                    "archived": false,
                    "default_branch": "main",
                    "pushed_at": "2026-11-25T00:00:00Z",
                    "html_url": "https://github.com/acme/api"
                },
                {
                    "full_name": "secretcorp/billing",
                    "owner": {"login": "secretcorp"},
                    "description": "outside-collaborator private repo",
                    "private": true,
                    "archived": false,
                    "default_branch": "main",
                    "pushed_at": "2026-11-24T00:00:00Z",
                    "html_url": "https://github.com/secretcorp/billing"
                },
                {
                    "full_name": "octo/legacy",
                    "owner": {"login": "octo"},
                    "description": "archived repo",
                    "private": false,
                    "archived": true,
                    "default_branch": "main",
                    "pushed_at": "2026-10-01T00:00:00Z",
                    "html_url": "https://github.com/octo/legacy"
                }
            ])),
        ),
        "/repos/octo/hello" => (
            200,
            "OK",
            json(serde_json::json!({
                "full_name": "octo/hello",
                "description": "demo",
                "private": false,
                "default_branch": "main",
                "html_url": "https://github.com/octo/hello"
            })),
        ),
        "/repos/octo/world" => (
            200,
            "OK",
            json(serde_json::json!({
                "full_name": "octo/world",
                "private": true,
                "default_branch": "main",
                "html_url": "https://github.com/octo/world"
            })),
        ),
        "/repos/octo/hello/pulls" => {
            // #5 is flagged merged: upstream would never return it
            // for state=open, but a stale cached page can carry it —
            // the aggregator must drop it. #11 appears only when the
            // fixture's `extra` flag is set (refresh-bypass test).
            let mut list = vec![
                pr(7, "Older fix", "2026-11-24T10:00:00Z", false),
                pr(9, "Newer fix", "2026-11-25T10:00:00Z", true),
                pr_full(5, "Merged one", "2026-11-23T10:00:00Z", false, true),
            ];
            if extra {
                list.push(pr(11, "Late PR", "2026-11-27T10:00:00Z", false));
            }
            (200, "OK", json(serde_json::json!(list)))
        }
        "/repos/octo/world/pulls" => (
            200,
            "OK",
            json(serde_json::json!([pr(
                3,
                "World PR",
                "2026-11-26T10:00:00Z",
                false
            ),])),
        ),
        "/repos/octo/hello/pulls/7" => (
            200,
            "OK",
            json(serde_json::json!({
                "number": 7,
                "title": "Older fix",
                "state": "open",
                "draft": false,
                "user": {"login": "octo"},
                "labels": [],
                "head": {"ref": "fix/7"},
                "base": {"ref": "main"},
                "created_at": "2026-11-20T10:00:00Z",
                "updated_at": "2026-11-24T10:00:00Z",
                "html_url": "https://github.com/octo/hello/pull/7",
                "additions": 12,
                "deletions": 3,
                "changed_files": 2,
                "comments": 4,
                "body": "the body",
            })),
        ),
        // CI check-runs per head SHA (used by ?enrich=true).
        "/repos/octo/hello/commits/sha9/check-runs" => (
            200,
            "OK",
            json(serde_json::json!({
                "total_count": 2,
                "check_runs": [
                    {"status": "completed", "conclusion": "success"},
                    {"status": "completed", "conclusion": "failure"}
                ]
            })),
        ),
        "/repos/octo/hello/commits/sha7/check-runs" => (
            200,
            "OK",
            json(serde_json::json!({
                "total_count": 1,
                "check_runs": [{"status": "completed", "conclusion": "success"}]
            })),
        ),
        // No check runs on world/sha3 → the client falls back to the
        // combined status API, which reports pending.
        "/repos/octo/world/commits/sha3/check-runs" => (
            200,
            "OK",
            json(serde_json::json!({"total_count": 0, "check_runs": []})),
        ),
        "/repos/octo/world/commits/sha3/status" => (
            200,
            "OK",
            json(serde_json::json!({
                "state": "pending",
                "total_count": 1,
                "statuses": [{"state": "pending", "context": "ci/tests"}]
            })),
        ),
        // octo/secret and anything else → 404 (no access / missing)
        _ => (
            404,
            "Not Found",
            json(serde_json::json!({"message": "Not Found"})),
        ),
    }
}

// ---------------------------------------------------------------------------
// App boot
// ---------------------------------------------------------------------------

/// Boot a test app wired with auth + the github ServiceDef, the
/// loopback fixture as `[agents.github].base_url`, and (when
/// `seed_vault`) the PAT + dashboard fields in the vault.
async fn boot_dashboard_app(
    fx: &Fixture,
    seed_vault: bool,
) -> (axum::Router, String, Arc<nagent_server::AppState>) {
    use nagent_agents::github_service::GITHUB_SERVICE;
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
            "sqlite://file:github_dash_e2e_app_{}?mode=memory&cache=shared",
            Uuid::new_v4()
        ),
        max_connections: 1,
        auto_migrate: true,
    };
    Arc::make_mut(&mut builder.config).agents.github.base_url = fx.base_url();
    Arc::make_mut(&mut builder.config).agents.github.allowlist = vec!["127.0.0.1".to_string()];
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
    let probe_repo_routes =
        nagent_server::credentials::github_probe_repo::build_github_probe_repo_router(
            state.clone(),
        );
    let api_routes = nagent_server::credentials::github_api::build_github_api_router(state.clone());
    let app = cred_routes
        .merge(probe_repo_routes)
        .merge(api_routes)
        .layer(auth_layer)
        .with_state(state.clone());

    if seed_vault {
        let resp = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .method("PUT")
                    .uri("/api/integrations/github/credentials")
                    .header(axum::http::header::AUTHORIZATION, &bearer)
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "fields": {
                                "personal_access_token": TEST_TOKEN,
                                "username": "octo",
                                "show_dashboard": "true",
                                "dashboard_repos": "[\"octo/hello\",\"octo/world\",\"octo/secret\"]"
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

    (app, bearer, state)
}

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

async fn post_json(
    app: axum::Router,
    bearer: &str,
    uri: &str,
    body: serde_json::Value,
) -> axum::response::Response {
    app.oneshot(
        HttpRequest::builder()
            .method("POST")
            .uri(uri)
            .header(axum::http::header::AUTHORIZATION, bearer)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
    .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn probe_repo_ok_404_400_and_missing_token() {
    let fx = Fixture::start().await;
    let (app, bearer, _state) = boot_dashboard_app(&fx, true).await;

    // Accessible repo → 200 with the projected summary.
    let resp = post_json(
        app.clone(),
        &bearer,
        "/api/integrations/github/probe-repo",
        serde_json::json!({"repo": "octo/hello"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["repo"]["full_name"], "octo/hello");
    assert_eq!(body["repo"]["default_branch"], "main");

    // Inaccessible / missing repo → 404 with the no-access message.
    let resp = post_json(
        app.clone(),
        &bearer,
        "/api/integrations/github/probe-repo",
        serde_json::json!({"repo": "octo/secret"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = body_json(resp).await;
    // Plan 1795793328177: the all-accounts-404 message names the
    // accounts tried — never a token (AGENTS.md §6).
    let err = body["error"].as_str().unwrap();
    assert!(
        err.contains("can read") && err.contains("default"),
        "404 lists the accounts tried: {err}"
    );
    assert!(!err.contains(TEST_TOKEN), "no token in the 404 body");

    // Malformed repo → 400 before any network call.
    let before = fx.requests().len();
    let resp = post_json(
        app.clone(),
        &bearer,
        "/api/integrations/github/probe-repo",
        serde_json::json!({"repo": "../etc/passwd"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        fx.requests().len(),
        before,
        "a malformed repo must be rejected before any network call"
    );

    // No saved PAT → 400 "not configured".
    let (app2, bearer2, _s2) = boot_dashboard_app(&fx, false).await;
    let resp = post_json(
        app2,
        &bearer2,
        "/api/integrations/github/probe-repo",
        serde_json::json!({"repo": "octo/hello"}),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = body_json(resp).await;
    assert!(body["error"].as_str().unwrap().contains("not configured"));
}

#[tokio::test]
async fn prs_list_aggregates_filters_and_isolates_repo_errors() {
    let fx = Fixture::start().await;
    let (app, bearer, _state) = boot_dashboard_app(&fx, true).await;

    // Full aggregation: 3 PRs across hello+world, sorted by
    // updated_at desc (world's PR is the newest), and octo/secret
    // isolated in repo_errors instead of failing the list.
    let resp = get(app.clone(), &bearer, "/api/integrations/github/prs").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = &body["data"];
    assert_eq!(data["repos"].as_array().unwrap().len(), 3);
    assert_eq!(data["state"], "open");
    assert_eq!(data["total"], 3);
    assert_eq!(data["count"], 3);
    let prs = data["prs"].as_array().unwrap();
    assert_eq!(prs[0]["number"], 3, "newest updated_at first");
    assert_eq!(prs[0]["repo"], "octo/world");
    assert_eq!(prs[1]["number"], 9);
    assert_eq!(prs[2]["number"], 7);
    assert_eq!(prs[1]["labels"][0]["name"], "bug");
    assert_eq!(prs[1]["head_ref"], "fix/9");
    assert_eq!(prs[1]["base_ref"], "main");
    let errs = data["repo_errors"].as_array().unwrap();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["repo"], "octo/secret");
    assert_eq!(errs[0]["status"], 404);

    // ?repo= filter to one configured repo.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/prs?repo=octo/hello",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let data = &body["data"];
    assert_eq!(data["filter"], "octo/hello");
    assert_eq!(data["count"], 2);
    assert!(data["prs"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["repo"] == "octo/hello"));

    // ?repo= on an unconfigured repo → 400.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/prs?repo=octo/nope",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Invalid state → 400.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/prs?state=merged",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Missing PAT → 400 on the list endpoint too.
    let (app2, bearer2, _s2) = boot_dashboard_app(&fx, false).await;
    let resp = get(app2, &bearer2, "/api/integrations/github/prs").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn prs_enrich_attaches_cached_ci_status() {
    let fx = Fixture::start().await;
    let (app, bearer, _state) = boot_dashboard_app(&fx, true).await;

    // Without ?enrich the rows carry no `ci` key.
    let resp = get(app.clone(), &bearer, "/api/integrations/github/prs").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert!(body["data"]["prs"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p.get("ci").is_none()));

    // With ?enrich=true each returned row carries a ci object:
    // world#3 → pending (status-API fallback), hello#9 → failure,
    // hello#7 → success; the url points at the PR's /checks tab.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/prs?enrich=true",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let prs = body["data"]["prs"].as_array().unwrap();
    assert_eq!(prs.len(), 3);
    let ci_of = |n: u64| {
        prs.iter()
            .find(|p| p["number"] == n)
            .and_then(|p| p.get("ci"))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    assert_eq!(ci_of(3)["state"], "pending");
    assert_eq!(ci_of(9)["state"], "failure");
    assert!(ci_of(9)["detail"].as_str().unwrap().contains("1 failure"));
    assert_eq!(ci_of(7)["state"], "success");
    assert!(ci_of(7)["url"]
        .as_str()
        .unwrap()
        .ends_with("/pull/7/checks"));

    // CI lookups are cached per (repo, sha): a second enriched call
    // must not re-hit the check-runs endpoints.
    let ci_calls = |fx: &Fixture| {
        fx.requests()
            .iter()
            .filter(|(l, _)| l.contains("/check-runs") || l.contains("/status"))
            .count()
    };
    let after_first = ci_calls(&fx);
    assert_eq!(after_first, 4, "3 check-runs + 1 status fallback");
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/prs?enrich=true",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        ci_calls(&fx),
        after_first,
        "the second enriched call must reuse the cached CI status"
    );
}

#[tokio::test]
async fn prs_refresh_bypasses_cache_and_open_list_drops_merged() {
    let fx = Fixture::start().await;
    let (app, bearer, _state) = boot_dashboard_app(&fx, true).await;

    // state=open drops rows flagged merged, even when a (stale)
    // cached page carries one (#5).
    let resp = get(app.clone(), &bearer, "/api/integrations/github/prs").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let prs = body["data"]["prs"].as_array().unwrap();
    assert_eq!(body["data"]["count"], 3);
    assert!(
        prs.iter().all(|p| p["number"] != 5),
        "merged PRs must be filtered out of the open list"
    );

    // A plain call is served from the response cache: flipping the
    // fixture (an 11th PR appears upstream) does not change it.
    fx.set_extra_pr(true);
    let resp = get(app.clone(), &bearer, "/api/integrations/github/prs").await;
    let body = body_json(resp).await;
    assert_eq!(body["data"]["count"], 3, "within TTL the cache answers");

    // ?refresh=true skips the cache read and writes the fresh page
    // through.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/prs?refresh=true",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["count"], 4, "refresh must bypass the cache");
    assert!(body["data"]["prs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["number"] == 11));

    // The next plain call sees the refreshed cache (write-through).
    let resp = get(app.clone(), &bearer, "/api/integrations/github/prs").await;
    let body = body_json(resp).await;
    assert_eq!(
        body["data"]["count"], 4,
        "refresh writes through to the cache"
    );
}

#[tokio::test]
async fn pr_detail_endpoint_is_gated_on_configured_repos() {
    let fx = Fixture::start().await;
    let (app, bearer, _state) = boot_dashboard_app(&fx, true).await;

    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/prs/octo/hello/7",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let pr = &body["data"]["pr"];
    assert_eq!(pr["number"], 7);
    assert_eq!(pr["repo"], "octo/hello");
    assert_eq!(pr["additions"], 12);
    assert_eq!(pr["comments"], 4);
    assert_eq!(pr["body"], "the body");

    // Repo not in dashboard_repos → 400 (never reaches upstream).
    let before = fx.requests().len();
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/prs/octo/nope/1",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fx.requests().len(), before);

    // Configured but inaccessible repo → upstream 404 surfaces as 404.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/prs/octo/secret/1",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn owners_and_owner_repos_endpoints_back_the_picker() {
    let fx = Fixture::start().await;
    let (app, bearer, _state) = boot_dashboard_app(&fx, true).await;

    // Owners: self + orgs from /user/orgs + owners only reachable
    // through accessible repos (secretcorp is an outside-
    // collaborator org, absent from /user/orgs).
    let resp = get(app.clone(), &bearer, "/api/integrations/github/owners").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["user"], "octo");
    let owners = body["data"]["owners"].as_array().unwrap();
    let via = |login: &str| {
        owners
            .iter()
            .find(|o| o["login"] == login)
            .and_then(|o| o["via"].as_str())
            .unwrap_or("<missing>")
    };
    assert_eq!(via("octo"), "self");
    assert_eq!(via("acme"), "org");
    assert_eq!(
        via("secretcorp"),
        "repo",
        "an owner reachable only through accessible (private) repos must be listed"
    );

    // The upstream source is /user/repos?visibility=all&affiliation=…
    // (visibility=all is what surfaces private repos) — `type` must
    // NOT be sent: GitHub answers 422 when it is combined with
    // visibility/affiliation. Never /orgs/{org}/repos.
    assert!(fx.requests().iter().any(|(l, _)| {
        l.starts_with("GET /user/repos?")
            && l.contains("visibility=all")
            && l.contains("affiliation=owner%2Ccollaborator%2Corganization_member")
            && l.contains("page=1")
            && !l.contains("type=")
    }));
    assert!(!fx
        .requests()
        .iter()
        .any(|(l, _)| l.starts_with("GET /orgs/")));

    // Private outside-collaborator repo of another org is listed.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/owners/secretcorp/repos",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["owner"], "secretcorp");
    assert_eq!(body["data"]["count"], 1);
    assert_eq!(body["data"]["repos"][0]["full_name"], "secretcorp/billing");
    assert_eq!(body["data"]["repos"][0]["private"], true);

    // Org repos (private included).
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/owners/acme/repos",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["count"], 1);
    assert_eq!(body["data"]["repos"][0]["full_name"], "acme/api");

    // Own repos only (filtered by owner.login); the archived
    // octo/legacy is hidden by default.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/owners/octo/repos",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["count"], 1);
    assert_eq!(body["data"]["repos"][0]["full_name"], "octo/hello");

    // ?include_archived=true surfaces the archived repo (and keeps
    // the non-archived one), cached under a distinct key.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/owners/octo/repos?include_archived=true",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["count"], 2);
    let names: Vec<&str> = body["data"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["full_name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"octo/hello"));
    assert!(names.contains(&"octo/legacy"));
    let legacy = body["data"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["full_name"] == "octo/legacy")
        .unwrap();
    assert_eq!(legacy["archived"], true);

    // Invalid owner name → 400 before any network call.
    let before = fx.requests().len();
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/owners/a..b/repos",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(fx.requests().len(), before);

    // Owner the PAT cannot see → 200 with an empty list (GitHub
    // enforces visibility upstream; nothing to 404 on).
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/owners/nope/repos",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["count"], 0);
}

#[tokio::test]
async fn owners_pagination_walks_pages_until_short_page_and_caches() {
    let fx = Fixture::start_paged().await;
    let (app, bearer, _state) = boot_dashboard_app(&fx, true).await;

    let resp = get(app.clone(), &bearer, "/api/integrations/github/owners").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let owners = body["data"]["owners"].as_array().unwrap();
    let via = |login: &str| {
        owners
            .iter()
            .find(|o| o["login"] == login)
            .and_then(|o| o["via"].as_str())
            .unwrap_or("<missing>")
    };
    assert_eq!(via("octo"), "self");
    // secretcorp only exists on page 2 of /user/repos: the walk must
    // have reached it.
    assert_eq!(via("secretcorp"), "repo");

    let user_repo_calls = |fx: &Fixture| {
        fx.requests()
            .iter()
            .filter(|(l, _)| l.starts_with("GET /user/repos?"))
            .cloned()
            .collect::<Vec<_>>()
    };
    let calls = user_repo_calls(&fx);
    assert!(calls.iter().any(|(l, _)| l.contains("page=1")));
    assert!(calls.iter().any(|(l, _)| l.contains("page=2")));
    assert!(
        !calls.iter().any(|(l, _)| l.contains("page=3")),
        "the walk must stop at the short page"
    );
    let calls_after_owners = calls.len();

    // Per-owner view reuses the cached merged list: no new upstream
    // call, and all 100 page-1 repos are visible for octo.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/owners/octo/repos",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["count"], 100);
    assert_eq!(body["data"]["repos"][0]["full_name"], "octo/repo-0");
    assert_eq!(
        user_repo_calls(&fx).len(),
        calls_after_owners,
        "the per-owner view must reuse the cached page walk"
    );

    // The page-2 private repos are reachable through their owners.
    let resp = get(
        app.clone(),
        &bearer,
        "/api/integrations/github/owners/secretcorp/repos",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["data"]["count"], 1);
    assert_eq!(body["data"]["repos"][0]["full_name"], "secretcorp/billing");
    assert_eq!(body["data"]["repos"][0]["private"], true);
}

#[tokio::test]
async fn dashboard_responses_never_carry_the_token() {
    let fx = Fixture::start().await;
    let (app, bearer, _state) = boot_dashboard_app(&fx, true).await;

    let mut bodies = Vec::new();
    for uri in [
        "/api/integrations/github/prs",
        "/api/integrations/github/prs?repo=octo/nope",
        "/api/integrations/github/prs/octo/secret/1",
        "/api/integrations/github/owners",
        "/api/integrations/github/owners/nope/repos",
    ] {
        let resp = get(app.clone(), &bearer, uri).await;
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        bodies.push(String::from_utf8_lossy(&bytes).into_owned());
    }
    for body in [
        serde_json::json!({"repo": "octo/secret"}),
        serde_json::json!({"repo": "../x"}),
    ] {
        let resp = post_json(
            app.clone(),
            &bearer,
            "/api/integrations/github/probe-repo",
            body,
        )
        .await;
        let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        bodies.push(String::from_utf8_lossy(&bytes).into_owned());
    }
    for b in &bodies {
        assert!(
            !b.contains(TEST_TOKEN),
            "AGENTS.md §6: the PAT must never appear in a response body; got {b}"
        );
    }
    // The fixture DID receive the bearer on the API calls (auth was
    // actually attempted) — redaction is about the response path.
    assert!(fx
        .requests()
        .iter()
        .any(|(_, auth)| auth == &format!("Bearer {TEST_TOKEN}")));
}

#[tokio::test]
async fn dashboard_fields_roundtrip_through_credentials_api() {
    let fx = Fixture::start().await;
    let (app, bearer, _state) = boot_dashboard_app(&fx, true).await;

    let resp = get(app.clone(), &bearer, "/api/integrations/github").await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["configured"], true);
    let fields = body["fields"].as_array().unwrap();
    let find = |key: &str| fields.iter().find(|f| f["key"] == key).cloned();
    let show = find("show_dashboard").expect("show_dashboard field");
    assert_eq!(show["kind"], "checkbox");
    assert_eq!(show["value"], "true", "checkbox state echoes on edit");
    let repos = find("dashboard_repos").expect("dashboard_repos field");
    assert_eq!(
        repos["value"],
        "[\"octo/hello\",\"octo/world\",\"octo/secret\"]"
    );
    let pat = find("personal_access_token").expect("PAT field");
    assert_eq!(pat["kind"], "password");
    assert!(
        pat.get("value").is_none(),
        "AGENTS.md §6: the PAT is never echoed, even to its owner"
    );
}
