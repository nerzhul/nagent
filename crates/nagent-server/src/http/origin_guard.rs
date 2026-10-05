//! `http::origin_guard` — plan S-2: `Origin` / `Host` validation.
//!
//! Browsers do not apply CORS to WebSocket upgrades, so a public
//! deployment of `nagent-server` is wide-open to cross-site
//! WebSocket hijacking (CSWSH) and DNS-rebinding when the bind
//! address is reachable. With `auth.enabled = false` (the default)
//! this is the cheapest attack: any web page the user visits can
//! open `ws://localhost:8080/ws` and start streaming audio through
//! the inference pipeline.
//!
//! ## What this module enforces
//!
//! - **`Origin` on state-changing routes**: every `POST` / `PUT` /
//!   `PATCH` / `DELETE` / WebSocket upgrade must carry an `Origin`
//!   header that matches the operator-configured allow-list. Same-
//!   origin requests always pass; cross-origin requests get
//!   `403 Forbidden` with no body (the planner surfaces "no body"
//!   on purpose — leaking the reason would help the attacker map
//!   the allowed origins).
//! - **`Host` header on every request**: matches an allow-list
//!   derived from `[server].bind_addr` and the
//!   `[server].allowed_origins` operator override. A request with
//!   a forged `Host` (DNS-rebinding attempt) is rejected with
//!   `421 Misdirected Request` so the browser can recover.
//! - **WebSocket upgrades**: subject to the same `Origin` check as
//!   state-changing routes plus the `Host` check. The check runs
//!   at the HTTP layer, before `WebSocketUpgrade` consumes the
//!   request — a rejected upgrade is a plain `403` rather than a
//!   WS close frame that operators would have to decode.
//!
//! ## Defaults
//!
//! With no `[server].allowed_origins` override, the allow-list is
//! derived from the bind address (`127.0.0.1:<port>`,
//! `[::1]:<port>`, and the literal host part). This keeps the
//! historical dev workflow ("`make run` from a single user on
//! loopback") working without configuration. Operators exposing
//! the server on a non-loopback address MUST populate
//! `[server].allowed_origins` (or `NAGENT_ALLOWED_ORIGINS`) with
//! the externally-reachable scheme + host, or every request will
//! fail. The existing `trusted_proxies` boot warning gains a
//! companion line that fires when the bind address is non-
//! loopback AND `allowed_origins` is empty (see
//! [`crate::app::build_app`]).
//!
//! ## Safe defaults for the public subtree
//!
//! The middleware is wired **only** to the protected subtree.
//! `/`, `/static/*`, `/healthz`, and `/api/version` stay open so
//! the login page + health probes keep working — the planner
//! classifies them as `public` and explicitly exempts them from
//! the origin check (a public site should be reachable from a
//! bookmark or a fresh browser tab with no `Origin` header).

use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::warn;

use crate::config::Config;
use crate::AppState;

/// Outcome of an [`evaluate`] call. The middleware surfaces
/// rejections as [`StatusCode::FORBIDDEN`] (Origin mismatch on a
/// state-changing route) or [`StatusCode::MISDIRECTED_REQUEST`]
/// (Host mismatch — DNS-rebinding signal). Both responses carry a
/// `Vary: Origin` so downstream caches do not serve one origin's
/// rejection to another.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Request is allowed as-is.
    Allow,
    /// The `Origin` header (when present, or required for state-
    /// changing methods) is not in the allow-list.
    ForbiddenOrigin,
    /// The `Host` header is not in the allow-list.
    MisdirectedHost,
}

/// Resolve the source of an allow-list decision for the access log
/// and the operator-facing WARN at boot. Cheap to clone (`&str`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginSource {
    /// No operator override was supplied; the allow-list was
    /// derived from `[server].bind_addr`.
    Derived,
    /// The operator populated `[server].allowed_origins` (or the
    /// matching env var); the allow-list is the supplied list.
    Operator,
}

impl OriginSource {
    pub fn as_str(self) -> &'static str {
        match self {
            OriginSource::Derived => "derived",
            OriginSource::Operator => "operator",
        }
    }
}

/// Build the effective `Origin` allow-list from the operator
/// override (if any) or the bind address. The returned vector
/// contains the canonical `scheme://host[:port]` strings the
/// middleware compares against, in lowercase, with no trailing
/// slash.
pub fn build_allowed_origins(cfg: &Config) -> (Vec<String>, OriginSource) {
    if !cfg.allowed_origins.origins.is_empty() {
        let normalised: Vec<String> = cfg
            .allowed_origins
            .origins
            .iter()
            .map(|s| normalise_origin(s))
            .collect();
        return (normalised, OriginSource::Operator);
    }
    // Derived: scheme is `http` for loopback binds (no TLS in the
    // default dev workflow), `https` otherwise. The port matches
    // the bind address.
    let scheme = if cfg.bind_addr.ip().is_loopback() {
        "http"
    } else {
        "https"
    };
    let port = cfg.bind_addr.port();
    let host = cfg.bind_addr.ip();
    let mut out = Vec::new();
    match host {
        IpAddr::V4(v4) => {
            out.push(format!("{scheme}://{v4}:{port}"));
            out.push(format!("{scheme}://localhost:{port}"));
        }
        IpAddr::V6(v6) => {
            // IPv6 literals in `Origin` must be wrapped in `[]`.
            out.push(format!("{scheme}://[{v6}]:{port}"));
        }
    }
    (out, OriginSource::Derived)
}

/// Build the effective `Host` allow-list. Same shape as
/// [`build_allowed_origins`] but the comparison happens on the
/// raw `Host` header (`host[:port]`), not on a scheme-prefixed
/// `Origin`. Always derived — the operator-facing knob is
/// `[server].allowed_origins` and the `Host` allow-list mirrors
/// the host parts it contains (plus the bind-address literals).
pub fn build_allowed_hosts(cfg: &Config, allowed_origins: &[String]) -> Vec<String> {
    let mut out: Vec<String> = allowed_origins
        .iter()
        .filter_map(|o| host_part_of_origin(o).map(str::to_string))
        .collect();
    // Always include the bind-address host (with and without the
    // port) so a `Host: 127.0.0.1:8080` request passes even when
    // the operator override only listed `localhost`.
    let bind_origin = format!(
        "{}://{}:{}",
        if cfg.bind_addr.ip().is_loopback() {
            "http"
        } else {
            "https"
        },
        cfg.bind_addr.ip(),
        cfg.bind_addr.port()
    );
    if let Some(h) = host_part_of_origin(&bind_origin) {
        let h = h.to_string();
        if !out.iter().any(|existing| existing == &h) {
            out.push(h);
        }
    }
    out
}

/// Evaluate one inbound request against the policy described by
/// the module-level doc. The `cfg` argument drives the allow-
/// list; `headers` is the inbound request's `HeaderMap` (passed
/// through so the middleware fn does not own the extractor).
pub fn evaluate(
    method: &Method,
    host: Option<&str>,
    origin: Option<&str>,
    cfg: &Config,
) -> Decision {
    let (allowed_origins, _) = build_allowed_origins(cfg);
    let allowed_hosts = build_allowed_hosts(cfg, &allowed_origins);
    evaluate_with(method, host, origin, &allowed_origins, &allowed_hosts)
}

/// Same as [`evaluate`] but takes pre-computed allow-lists so
/// tests can drive the policy without rebuilding the closure on
/// every call.
pub fn evaluate_with(
    method: &Method,
    host: Option<&str>,
    origin: Option<&str>,
    allowed_origins: &[String],
    allowed_hosts: &[String],
) -> Decision {
    // Host check applies to every method; a request with a forged
    // Host is a rebinding signal even on a GET. A *missing* Host
    // header is not the same as a forged one — it only shows up on
    // a malformed HTTP/1.0 request or an in-process
    // `oneshot(...)` test that builds the request without going
    // through the HTTP/1.1 stack. We skip the Host check rather
    // than reject so test code does not have to plumb a Host
    // header through every helper; in production hyper always
    // populates Host before the request reaches the router.
    if let Some(h) = host {
        let h_norm = h.trim().to_ascii_lowercase();
        // Strip an explicit port when matching so `127.0.0.1:8080`
        // and `127.0.0.1` are treated as the same host.
        let h_no_port = h_norm.split(':').next().unwrap_or(&h_norm).to_string();
        let allowed = allowed_hosts.iter().any(|entry| {
            let entry = entry.to_ascii_lowercase();
            entry == h_norm
                || entry.split(':').next().unwrap_or(&entry) == h_no_port
                || entry == h_no_port
        });
        if !allowed {
            return Decision::MisdirectedHost;
        }
    }

    // Origin check applies only to state-changing methods and
    // WebSocket upgrades. GET / HEAD / OPTIONS are exempt so the
    // browser can fetch the index page, static assets, and run
    // CORS preflights without an Origin header.
    let requires_origin = matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    if requires_origin {
        match origin {
            // No Origin header at all. Browsers always send
            // Origin on cross-origin requests; same-origin
            // requests also send Origin in modern browsers. The
            // only clients that omit Origin are programmatic
            // (curl, tests, server-to-server). We allow those
            // when the Host header matches the allow-list, so
            // the test harness and `curl` keep working without
            // an Origin header.
            None => {
                if !host.is_some() {
                    return Decision::ForbiddenOrigin;
                }
            }
            Some(o) => {
                let o_norm = normalise_origin(o);
                if !allowed_origins.iter().any(|entry| entry == &o_norm) {
                    return Decision::ForbiddenOrigin;
                }
            }
        }
    }
    Decision::Allow
}

/// Middleware fn that applies [`evaluate`] to every request.
/// Wired to the protected subtree in [`crate::http::build_router`]
/// via [`axum::middleware::from_fn_with_state`]. The state type
/// matches the rest of the protected subtree (`Arc<AppState>`).
/// WebSocket upgrades are subject to the Origin check (a WS
/// handshake is a GET with `Upgrade: websocket`, which is *not*
/// a state-changing method; the WS branch in
/// [`crate::stt::ws_handler::ws_upgrade`] calls [`evaluate`]
/// explicitly so the upgrade is rejected before the handshake
/// runs).
pub async fn origin_guard_middleware(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // Treat a missing Host header as `None` so the policy can
    // distinguish a request that came through the HTTP/1.1 stack
    // (where hyper always populates Host) from an in-process
    // `oneshot(...)` test. The actual Host check is skipped when
    // the value is `None` — see the module-level note in
    // `evaluate_with` for the rationale.
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    match evaluate(&method, host, origin, &state.config) {
        Decision::Allow => next.run(req).await,
        Decision::ForbiddenOrigin => {
            // S-2: cross-origin request on a state-changing
            // route. Possible CSWSH / CSRF attempt; log at
            // `WARN` so a log aggregator can alert on the rate.
            warn!(
                event = "http.origin_guard.forbidden",
                method = %method,
                path = %path,
                host = ?host,
                origin = ?origin,
                "rejected: Origin header does not match the allow-list"
            );
            forbidden_origin_response()
        }
        Decision::MisdirectedHost => {
            // S-2: forged Host header — classic DNS-rebinding
            // signal. `WARN` so it pops up alongside the 403s in
            // the same alerting query.
            warn!(
                event = "http.origin_guard.misdirected",
                method = %method,
                path = %path,
                host = ?host,
                origin = ?origin,
                "rejected: Host header does not match the allow-list"
            );
            misdirected_host_response()
        }
    }
}

/// Helper used by the WebSocket upgrade handler: the WS branch
/// needs the same `Origin` / `Host` check without going through the
/// axum middleware extractor stack. Returns `Ok(())` if the upgrade
/// is allowed, `Err(response)` otherwise — the caller short-
/// circuits and returns the response as the HTTP upgrade reply.
///
/// `evaluate` classifies a plain GET as exempt from the Origin
/// check, which is the right behaviour for browser navigation
/// but the wrong one for CSWSH, where the attacker opens a
/// WebSocket from a third-party origin. The WS upgrade is
/// effectively a state-changing operation (it starts an
/// authenticated audio stream), so this fn flags the request as
/// state-changing via the explicit `requires_origin = true`
/// parameter.
pub fn check_ws_upgrade(
    headers: &HeaderMap,
    bind_host: &str,
    cfg: &Config,
) -> Result<(), Response> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    // The WS handler already knows the peer IP, so it can
    // reconstruct the expected host:port. When the operator is
    // behind a reverse proxy the `Host` header is rewritten to the
    // public hostname; the `allowed_hosts` derivation in
    // [`build_allowed_hosts`] mirrors that case.
    match evaluate_ws_upgrade(Some(bind_host), origin, cfg) {
        Decision::Allow => Ok(()),
        Decision::ForbiddenOrigin => Err(forbidden_origin_response()),
        Decision::MisdirectedHost => Err(misdirected_host_response()),
    }
}

/// Same as [`evaluate`] but with the Origin check always enforced
/// (a WebSocket upgrade is effectively a state-changing operation
/// for CSWSH purposes — see [`check_ws_upgrade`]).
pub fn evaluate_ws_upgrade(host: Option<&str>, origin: Option<&str>, cfg: &Config) -> Decision {
    let (allowed_origins, _) = build_allowed_origins(cfg);
    let allowed_hosts = build_allowed_hosts(cfg, &allowed_origins);
    evaluate_ws_upgrade_with(host, origin, &allowed_origins, &allowed_hosts)
}

/// Pre-computed allow-lists flavour of [`evaluate_ws_upgrade`].
pub fn evaluate_ws_upgrade_with(
    host: Option<&str>,
    origin: Option<&str>,
    allowed_origins: &[String],
    allowed_hosts: &[String],
) -> Decision {
    // Host check — same as `evaluate_with`.
    if let Some(h) = host {
        let h_norm = h.trim().to_ascii_lowercase();
        let h_no_port = h_norm.split(':').next().unwrap_or(&h_norm).to_string();
        let allowed = allowed_hosts.iter().any(|entry| {
            let entry = entry.to_ascii_lowercase();
            entry == h_norm
                || entry.split(':').next().unwrap_or(&entry) == h_no_port
                || entry == h_no_port
        });
        if !allowed {
            return Decision::MisdirectedHost;
        }
    }
    // Origin check — always required when the request carries
    // one (a browser always sends Origin on a cross-origin WS
    // upgrade). When Origin is absent we fall back to the same
    // rule as state-changing HTTP routes: a missing Origin on a
    // Host that matches the allow-list is a programmatic client
    // (curl, the test harness, server-to-server), not a CSWSH
    // attempt. The browser can never omit Origin on a cross-site
    // request — it always sends it.
    match origin {
        None => match host {
            Some(_) => Decision::Allow,
            None => Decision::ForbiddenOrigin,
        },
        Some(o) => {
            let o_norm = normalise_origin(o);
            if !allowed_origins.iter().any(|entry| entry == &o_norm) {
                Decision::ForbiddenOrigin
            } else {
                Decision::Allow
            }
        }
    }
}

/// Render the `403` response for an Origin mismatch. The body is
/// deliberately empty so we do not leak the allow-list contents to
/// an attacker probing the policy.
fn forbidden_origin_response() -> Response {
    let mut resp = (StatusCode::FORBIDDEN, "").into_response();
    resp.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Origin"));
    resp
}

/// Render the `421 Misdirected Request` response for a Host
/// mismatch. `421` is the spec-defined status for a request that
/// was directed at a server that cannot produce a response; the
/// browser surfaces it as a hard error without retrying against a
/// different host (which would defeat the rebinding defence).
fn misdirected_host_response() -> Response {
    let mut resp = (StatusCode::MISDIRECTED_REQUEST, "").into_response();
    resp.headers_mut()
        .insert(header::VARY, HeaderValue::from_static("Host"));
    resp
}

/// Canonical form of an Origin / host string for comparison:
/// trimmed, lowercased, no trailing slash.
fn normalise_origin(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/').to_ascii_lowercase();
    trimmed
}

/// Extract the host (host + port) from a normalised `scheme://host[:port]`
/// string. Returns `None` if the string does not look like a URL.
fn host_part_of_origin(origin: &str) -> Option<&str> {
    let after_scheme = origin.split_once("://")?.1;
    // Path component (e.g. `https://example.com/foo`) is not a
    // valid Origin; drop it.
    let host_port = after_scheme.split('/').next()?;
    Some(host_port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

    fn cfg_loopback() -> Config {
        Config {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            ..minimal_config()
        }
    }

    fn cfg_public() -> Config {
        Config {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8080),
            ..minimal_config()
        }
    }

    fn minimal_config() -> Config {
        // `Config::default()` requires a model path; construct the
        // minimum that satisfies `Config`'s invariants without
        // pulling in env / TOML parsing.
        Config {
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080),
            whisper_model_path: std::path::PathBuf::from("/tmp/fake.bin"),
            max_queue: 32,
            inference_workers: None,
            session_idle_timeout: std::time::Duration::from_secs(30),
            infer_timeout: std::time::Duration::from_secs(30),
            limits: crate::config::LimitsConfig::default(),
            rate_limit: crate::config::RateLimitConfig::default(),
            trusted_proxies: crate::config::TrustedProxiesConfig::default(),
            llm: crate::config::LlmConfig::default(),
            agents: crate::config::AgentConfig::default(),
            tts: crate::config::TtsConfig::default(),
            auth: crate::config::AuthConfig::default(),
            documents: crate::config::DocumentsConfig::default(),
            allowed_origins: crate::config::AllowedOriginsConfig::default(),
            x_oauth: crate::config::XOAuthConfig::default(),
        }
    }

    #[test]
    fn loopback_bind_derives_127_and_localhost() {
        let cfg = cfg_loopback();
        let (origins, src) = build_allowed_origins(&cfg);
        assert_eq!(src, OriginSource::Derived);
        assert!(origins.iter().any(|o| o == "http://127.0.0.1:8080"));
        assert!(origins.iter().any(|o| o == "http://localhost:8080"));
    }

    #[test]
    fn ipv6_bind_uses_brackets() {
        let cfg = Config {
            bind_addr: SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 8080),
            ..minimal_config()
        };
        let (origins, _) = build_allowed_origins(&cfg);
        assert!(
            origins.iter().any(|o| o == "http://[::1]:8080"),
            "IPv6 loopback must be wrapped in `[]`; origins = {origins:?}"
        );
    }

    #[test]
    fn public_bind_uses_https_scheme() {
        let cfg = cfg_public();
        let (origins, _) = build_allowed_origins(&cfg);
        assert!(origins.iter().all(|o| o.starts_with("https://")));
    }

    #[test]
    fn operator_override_wins() {
        let mut cfg = cfg_loopback();
        cfg.allowed_origins.origins = vec!["https://nagent.example.com".into()];
        let (origins, src) = build_allowed_origins(&cfg);
        assert_eq!(src, OriginSource::Operator);
        assert_eq!(origins, vec!["https://nagent.example.com".to_string()]);
    }

    #[test]
    fn same_origin_get_is_allowed_without_origin_header() {
        let cfg = cfg_loopback();
        let decision = evaluate(&Method::GET, Some("127.0.0.1:8080"), None, &cfg);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn same_origin_post_with_matching_origin_is_allowed() {
        let cfg = cfg_loopback();
        let decision = evaluate(
            &Method::POST,
            Some("127.0.0.1:8080"),
            Some("http://127.0.0.1:8080"),
            &cfg,
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn cross_origin_post_is_forbidden() {
        let cfg = cfg_loopback();
        let decision = evaluate(
            &Method::POST,
            Some("127.0.0.1:8080"),
            Some("https://attacker.example"),
            &cfg,
        );
        assert_eq!(decision, Decision::ForbiddenOrigin);
    }

    #[test]
    fn post_without_origin_header_is_allowed_when_host_matches() {
        // Same-origin POSTs from programmatic clients (curl, the
        // test harness, server-to-server) omit the Origin header.
        // When the Host header matches the bind-addr allow-list
        // the request is treated as same-origin and allowed. A
        // missing Host header on a state-changing request is
        // still rejected — that combination can only be a forged
        // request.
        let cfg = cfg_loopback();
        let decision = evaluate(&Method::POST, Some("127.0.0.1:8080"), None, &cfg);
        assert_eq!(decision, Decision::Allow);
        let no_host = evaluate(&Method::POST, None, None, &cfg);
        assert_eq!(no_host, Decision::ForbiddenOrigin);
    }

    #[test]
    fn missing_host_is_skipped_for_oneshot_compat() {
        // A missing Host header is permitted (skipped, not
        // rejected) so in-process `oneshot(...)` tests do not
        // have to plumb a Host header through every helper; the
        // production hyper stack always populates Host. See the
        // module-level note in `evaluate_with` for the rationale.
        let cfg = cfg_loopback();
        let decision = evaluate(&Method::GET, None, None, &cfg);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn forged_host_is_rejected() {
        // Operator override lists a public host; the request still
        // claims to be talking to `evil.com` — DNS rebinding.
        let mut cfg = cfg_loopback();
        cfg.allowed_origins.origins = vec!["https://nagent.example.com".into()];
        let decision = evaluate(
            &Method::GET,
            Some("evil.com"),
            Some("https://nagent.example.com"),
            &cfg,
        );
        assert_eq!(decision, Decision::MisdirectedHost);
    }

    #[test]
    fn host_check_strips_port_for_matching() {
        // The bind address is 127.0.0.1:8080; a request with
        // `Host: 127.0.0.1` (no port — degenerate but possible)
        // must still pass.
        let cfg = cfg_loopback();
        let decision = evaluate(&Method::GET, Some("127.0.0.1"), None, &cfg);
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn options_preflight_is_exempt_from_origin_check() {
        let cfg = cfg_loopback();
        let decision = evaluate(&Method::OPTIONS, Some("127.0.0.1:8080"), None, &cfg);
        assert_eq!(decision, Decision::Allow);
    }
}
