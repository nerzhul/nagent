//! WebSocket upgrade handler and per-connection task.
//!
//! One Tokio task per active connection. The task owns:
//! - the `WebSocket` stream,
//! - the receiving end of the session's outbound channel,
//! - the sending end of the worker job channel (shared with all sessions).
//!
//! The task is the only place that ever holds the outbound receiver for
//! its session; the `ResultRouter` only ever sees the sender.

use std::sync::Arc;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use std::net::{IpAddr, SocketAddr};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use stt_core::{InferRequest, InferenceJob};
use stt_proto::{decode_frame, encode_frame, BackendInfo, ErrorMessage, Payload};

// Static asset serving is handled by `crate::http::static_assets::serve`
// (plan R8a: zero-copy + ETag + Cache-Control + If-None-Match -> 304).
use crate::rate_limit::RateLimitError;
use crate::stt::result_router::send_to_session;
use crate::stt::session::{register, unregister, OutboundMessage};
use crate::stt::validation::FrameError;
use crate::stt::ws_concurrency::AdmitDecision;
use crate::version::VersionInfo;
use crate::SttState;

/// Handle a WebSocket upgrade request.
///
/// One token is consumed from the STT rate limiter keyed by the peer
/// IP. A bucket miss is reported as `429 Too Many Requests` on the
/// HTTP upgrade handshake rather than as a WS close, so the operator
/// can see the rejection in plain HTTP logs without having to decode
/// the close frame.
pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(bundle): State<crate::state::SttWithConfig>,
    headers: HeaderMap,
    peer: Option<ConnectInfo<SocketAddr>>,
) -> impl IntoResponse {
    // Plan S-2: validate `Origin` / `Host` before any token is
    // consumed, so a cross-site WebSocket hijack attempt is
    // rejected at the HTTP layer (plain 403 / 421) instead of
    // being silently admitted and then aborted later. The same
    // policy is also applied via the HTTP middleware to the
    // rest of the protected subtree; we duplicate the call here
    // because the WS upgrade is GET-with-Upgrade, which the
    // middleware does not gate on Origin — the WS branch needs
    // the explicit pre-check.
    //
    // The Host check needs the configured bind address, not the
    // peer's ephemeral source port. When the peer is absent
    // (in-process test) we fall back to the bind address as a
    // placeholder so the comparison still has a value to test.
    let bind_host = format!(
        "{}:{}",
        bundle.config.bind_addr.ip(),
        bundle.config.bind_addr.port()
    );
    if let Err(resp) =
        crate::http::origin_guard::check_ws_upgrade(&headers, &bind_host, &bundle.config)
    {
        // `check_ws_upgrade` has already produced a 403/421; the
        // middleware-side duplicate will log `http.origin_guard.*`
        // for cross-origin POSTs, but the WS branch logs here
        // too because the middleware does not run on the WS
        // upgrade path (a GET with `Upgrade: websocket` is
        // exempt from the middleware's Origin check on purpose,
        // and the explicit pre-check produces the actual
        // rejection).
        let status = resp.status();
        let reason = match status {
            StatusCode::FORBIDDEN => "forbidden_origin",
            StatusCode::MISDIRECTED_REQUEST => "misdirected_host",
            other => other.canonical_reason().unwrap_or("origin_guard_rejected"),
        };
        let origin = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok());
        warn!(
            event = "ws.upgrade.rejected",
            reason = reason,
            status = status.as_u16(),
            host = %bind_host,
            origin = ?origin,
            peer_ip = ?peer.as_ref().map(|p| p.0.ip()),
            "ws upgrade rejected by origin / host guard"
        );
        return resp;
    }

    let peer_ip = peer.map(|ConnectInfo(addr)| addr.ip());
    // Security plan #5: resolve the source IP through
    // `X-Forwarded-For` when the TCP peer is a trusted proxy.
    // This means ingress-nginx (or any other reverse proxy
    // added to `trusted_proxies.cidr`) ends up bucketing real
    // client IPs, not the ingress pod IP.
    let client_ip = peer_ip.map(|peer_ip| {
        crate::rate_limit::resolve_client_ip(&headers, peer_ip, &bundle.config.trusted_proxies)
    });
    let limiter = bundle.stt.rate_limiter.clone();
    let ws_concurrency = bundle.stt.ws_concurrency.clone();
    let stt_for_conn = Arc::clone(&bundle.stt);
    let config_for_conn = bundle.config.clone();

    // Decide whether to upgrade before consuming the upgrade itself;
    // this way a rejection is a plain 429 (no WS handshake started).
    match client_ip {
        Some(ip) => match limiter.check(ip) {
            Ok(()) => {
                // Reserve one WebSocket concurrency slot (global +
                // per-IP) before starting the handshake; a saturated
                // cap is reported as `503 + Retry-After` so the
                // rejection is visible in normal HTTP logs without
                // having to decode a WS close frame (plan S-1).
                match ws_concurrency.try_admit(Some(ip)) {
                    AdmitDecision::Admitted => ws
                        .on_upgrade(move |socket| {
                            ws_connection(socket, stt_for_conn, config_for_conn, Some(ip))
                        })
                        .into_response(),
                    AdmitDecision::GlobalFull => {
                        warn!(
                            event = "ws.upgrade.rejected",
                            reason = "global_concurrency_cap",
                            global_in_use = ws_concurrency.global_in_use(),
                            global_cap = ws_concurrency.global_cap(),
                            peer_ip = %ip,
                            "ws upgrade rejected: global concurrency cap reached"
                        );
                        ws_concurrency_full_response()
                    }
                    AdmitDecision::PerIpFull => {
                        warn!(
                            event = "ws.upgrade.rejected",
                            reason = "per_ip_concurrency_cap",
                            per_ip_in_use = ws_concurrency.per_ip_in_use(ip),
                            per_ip_cap = ws_concurrency.per_ip_cap(),
                            peer_ip = %ip,
                            "ws upgrade rejected: per-IP concurrency cap reached"
                        );
                        ws_concurrency_full_response()
                    }
                }
            }
            Err(RateLimitError::Limited { retry_after_ms, .. }) => {
                let secs = retry_after_ms.div_ceil(1000).max(1);
                warn!(
                    event = "ws.upgrade.rejected",
                    reason = "stt_rate_limit",
                    peer_ip = ?client_ip,
                    retry_after_ms = retry_after_ms,
                    "ws upgrade rejected: STT rate limit exceeded"
                );
                let mut resp = (
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate limit exceeded for STT pipeline",
                )
                    .into_response();
                resp.headers_mut().insert(
                    axum::http::header::RETRY_AFTER,
                    axum::http::HeaderValue::from_str(&secs.to_string())
                        .unwrap_or(axum::http::HeaderValue::from_static("1")),
                );
                resp
            }
        },
        // No peer address (in-process call, some test harnesses):
        // bypass the limiter. The rate-limit code path is unit-tested
        // independently.
        None => match ws_concurrency.try_admit(None) {
            AdmitDecision::Admitted => ws
                .on_upgrade(move |socket| {
                    ws_connection(socket, stt_for_conn, config_for_conn, None)
                })
                .into_response(),
            AdmitDecision::GlobalFull => {
                warn!(
                    event = "ws.upgrade.rejected",
                    reason = "global_concurrency_cap",
                    global_in_use = ws_concurrency.global_in_use(),
                    global_cap = ws_concurrency.global_cap(),
                    "ws upgrade rejected: global concurrency cap reached"
                );
                ws_concurrency_full_response()
            }
            AdmitDecision::PerIpFull => {
                warn!(
                    event = "ws.upgrade.rejected",
                    reason = "per_ip_concurrency_cap",
                    "ws upgrade rejected: per-IP concurrency cap reached"
                );
                ws_concurrency_full_response()
            }
        },
    }
}

/// Build the `503 + Retry-After: 1` response returned when the
/// WebSocket concurrency caps are saturated (plan S-1). Kept in a
/// helper so both branches (with and without a resolved peer IP)
/// stay in sync.
fn ws_concurrency_full_response() -> axum::response::Response {
    let mut resp = (
        StatusCode::SERVICE_UNAVAILABLE,
        "STT WebSocket concurrency cap reached; retry shortly",
    )
        .into_response();
    resp.headers_mut().insert(
        axum::http::header::RETRY_AFTER,
        axum::http::HeaderValue::from_static("1"),
    );
    resp
}

/// Per-connection task: loops between inbound frames and outbound messages.
///
/// `peer_ip`, when present, is used by the per-frame rate-limit check
/// to debounce flooding clients. When absent (in-process test harness
/// without a real TCP listener), per-frame checks are skipped — the
/// HTTP upgrade gate still applied whenever a real peer address was
/// available.
pub async fn ws_connection(
    socket: WebSocket,
    stt: Arc<SttState>,
    config: Arc<crate::config::Config>,
    peer_ip: Option<IpAddr>,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let limiter = stt.rate_limiter.clone();

    // Register BEFORE any send so we know our session ID and the outbound
    // channel is owned exclusively by this task.
    let (session_id, _state, mut outbound_rx) = register(&stt.sessions, 64);

    info!(%session_id, "ws connection opened");

    // Send BackendInfo immediately so the client knows which model/GPU is in use.
    let info = BackendInfo {
        model_id: stt.backend.model_id().to_string(),
        gpu_backend: stt.backend.backend_name().to_string(),
    };
    match encode_frame(&Payload::Backend(info)) {
        Ok(bytes) => {
            if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                warn!(%session_id, "failed to send BackendInfo, closing");
                unregister(&stt.sessions, session_id);
                return;
            }
        }
        Err(e) => {
            error!(%session_id, "encode BackendInfo failed: {e}");
        }
    }

    loop {
        tokio::select! {
            // ---- Inbound from the client ------------------------------------
            inbound = ws_rx.next() => {
                match inbound {
                    Some(Ok(Message::Binary(buf))) => {
                        // Consume one STT token per inbound WS frame.
                        // The HTTP-level check at upgrade time already
                        // deducted one token for the connection itself;
                        // per-frame checks protect against a single
                        // misbehaving session flooding the inference
                        // queue after upgrade.
                        if let Err(e) = limiter.check_opt(peer_ip) {
                            warn!(
                                event = "ws.frame.rejected",
                                reason = "stt_per_frame_rate_limit",
                                session_id = %session_id,
                                peer_ip = ?peer_ip,
                                error = %e,
                                "ws inbound rate-limited"
                            );
                            let err_payload = Payload::Error(ErrorMessage {
                                code: 1,
                                message: format!("{e}"),
                            });
                            if let Ok(bytes) = encode_frame(&err_payload) {
                                let _ = ws_tx.send(Message::Binary(bytes)).await;
                            }
                            let close = CloseFrame {
                                code: axum::extract::ws::close_code::POLICY,
                                reason: "rate limit exceeded".into(),
                            };
                            let _ = ws_tx.send(Message::Close(Some(close))).await;
                            break;
                        }
                        if let Err(e) = handle_inbound(&stt, &config, session_id, &buf).await {
                            // Frame validation rejections carry an
                            // `event = "ws.frame.rejected"` tag and
                            // the `error` field with the human-readable
                            // reason so a log aggregator can filter on
                            // the rejection reason without parsing the
                            // message text.
                            let reason = match &e {
                                InboundError::Validation(_) => "frame_validation",
                                InboundError::Codec(_) => "codec_decode",
                                InboundError::ClientStop => "client_stop",
                                InboundError::WorkerUnavailable => "worker_unavailable",
                            };
                            warn!(
                                event = "ws.frame.rejected",
                                reason = reason,
                                session_id = %session_id,
                                error = %e,
                                "ws inbound frame rejected"
                            );
                            if matches!(e, InboundError::ClientStop) {
                                break;
                            }
                            let err_payload = Payload::Error(ErrorMessage {
                                code: 1,
                                message: format!("{e}"),
                            });
                            if let Ok(bytes) = encode_frame(&err_payload) {
                                if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                                    warn!(
                                        event = "ws.frame.send_failed",
                                        session_id = %session_id,
                                        "ws send error frame failed"
                                    );
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(reason))) => {
                        debug!(%session_id, ?reason, "client sent Close");
                        break;
                    }
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                        // axum handles ping/pong automatically; nothing to do.
                    }
                    Some(Ok(Message::Text(_))) => {
                        warn!(
                            event = "ws.frame.rejected",
                            reason = "unexpected_text_frame",
                            session_id = %session_id,
                            "unexpected text frame"
                        );
                    }
                    Some(Err(e)) => {
                        warn!(%session_id, "ws error: {e}");
                        break;
                    }
                    None => break,
                }
            }

            // ---- Outbound from the server -----------------------------------
            outbound = outbound_rx.recv() => {
                match outbound {
                    Some(OutboundMessage::Payload(p)) => {
                        match encode_frame(&p) {
                            Ok(bytes) => {
                                if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                                    warn!(%session_id, "ws send failed");
                                    break;
                                }
                            }
                            Err(e) => {
                                error!(%session_id, "encode failed: {e}");
                            }
                        }
                    }
                    Some(OutboundMessage::Close) => {
                        let _ = ws_tx.send(Message::Close(None)).await;
                        break;
                    }
                    None => break,
                }
            }
        }
    }

    // Connection ended — remove ourselves from the map so subsequent
    // results are routed into a dead channel instead of being dropped,
    // and release the WebSocket concurrency slot reserved at the
    // HTTP upgrade (plan S-1).
    unregister(&stt.sessions, session_id);
    match peer_ip {
        Some(ip) => stt.ws_concurrency.release_with_ip(ip),
        None => stt.ws_concurrency.release_global(),
    }
    info!(%session_id, "ws connection closed");
}

/// Decode an inbound frame and dispatch it.
async fn handle_inbound(
    stt: &Arc<SttState>,
    config: &Arc<crate::config::Config>,
    session_id: Uuid,
    bytes: &[u8],
) -> Result<(), InboundError> {
    let payload = decode_frame(bytes).map_err(InboundError::Codec)?;

    match payload {
        Payload::Start(start) => {
            debug!(%session_id, ?start, "StartSession");
            // Validate sample rate and language hint before mutating
            // any session state. A bad client must not be able to
            // sneak an oversized lang string into the worker-side
            // language cache.
            FrameError::validate_start(
                &config.limits,
                start.sample_rate,
                start.lang_hint.as_deref(),
            )
            .map_err(InboundError::Validation)?;
            // Snapshot the Arc out of the DashMap shard before any await.
            let state_arc = stt.sessions.get(&session_id).map(|s| s.value().clone());
            if let Some(state) = state_arc {
                state.touch();
                state.set_language(normalize_lang_hint(start.lang_hint));
            }
        }
        Payload::Stop(_) => {
            debug!(%session_id, "StopSession");
            return Err(InboundError::ClientStop);
        }
        Payload::Config(cfg) => {
            debug!(%session_id, ?cfg, "Config");
            FrameError::validate_config(&config.limits, cfg.language.as_deref())
                .map_err(InboundError::Validation)?;
            let state_arc = stt.sessions.get(&session_id).map(|s| s.value().clone());
            if let Some(state) = state_arc {
                state.touch();
                state.set_language(normalize_lang_hint(cfg.language));
                state.set_translate(cfg.translate);
            }
        }
        Payload::Audio(audio) => {
            FrameError::validate_audio(&config.limits, &audio.samples)
                .map_err(InboundError::Validation)?;
            // Snapshot config from the session before we send the job;
            // never hold a DashMap shard lock across an .await.
            let snapshot = stt.sessions.get(&session_id).map(|s| s.value().clone());
            let Some(state) = snapshot else {
                debug!(%session_id, "session already gone, dropping audio");
                return Ok(());
            };
            state.touch();
            let language = state.language();
            let translate = state.translate();

            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            let job = InferenceJob {
                request: InferRequest {
                    session_id,
                    samples: Arc::new(audio.samples),
                    language,
                    translate,
                },
                response_tx: resp_tx,
            };
            // If the targeted worker's queue is full (or the pool has
            // been shut down) the send fails — surface an error to the
            // client so it knows the request did not reach a worker.
            stt.job_tx
                .send(job)
                .await
                .map_err(|_| InboundError::WorkerUnavailable)?;

            // Spawn a small task that awaits the result and pushes it through
            // the result router (which looks up the session by ID).
            let map = Arc::clone(&stt.sessions);
            tokio::spawn(async move {
                match resp_rx.await {
                    Ok(resp) => {
                        let payload = Payload::Final(stt_proto::FinalTranscript {
                            text: resp.text,
                            segments: resp.segments,
                            lang: resp.lang,
                        });
                        if !send_to_session(&map, resp.session_id, payload).await {
                            debug!(%resp.session_id, "session gone, result dropped");
                        }
                    }
                    Err(_) => {
                        debug!(session_id = %session_id, "oneshot dropped before response");
                    }
                }
            });
        }
        _ => {
            // The server ignores client-sent transcript/error/backend payloads.
            debug!(?payload, "ignoring server-only payload from client");
        }
    }
    Ok(())
}

/// Lower-case the language hint so the worker-side cache and the
/// wire-side `lang` field report a single canonical form. Validation
/// already filtered out unknown codes; this is purely cosmetic.
fn normalize_lang_hint(hint: Option<String>) -> Option<String> {
    hint.map(|s| s.to_ascii_lowercase())
}

#[derive(Debug, thiserror::Error)]
enum InboundError {
    #[error("codec: {0}")]
    Codec(#[from] stt_proto::CodecError),
    #[error("client requested stop")]
    ClientStop,
    #[error("worker queue closed")]
    WorkerUnavailable,
    #[error("{0}")]
    Validation(#[from] FrameError),
}

/// Static handler for `/` and `/index.html`. Feature flags
/// (e.g. documents enabled) are now served via `GET /api/features`
/// instead of being inlined into the HTML — the browser fetches
/// the endpoint once after `/api/me` succeeds and uses the JSON
/// to drive UI visibility (see `static/documents.js`). Inlining
/// was CSP-hostile (every inline `<script>` needed a nonce) and
/// required a per-request string copy of the index HTML; the
/// endpoint approach is cleaner and survives the same cached
/// `index.html` for every state.
///
/// Plan R8a: respects `If-None-Match` against the per-file ETag so
/// repeat visits land on a `304 Not Modified` instead of a full
/// re-download. The `index_handler` does NOT inject the
/// `nagentConfig` block — that lives in [`index_with_config_handler`].
pub async fn index_handler(headers: axum::http::HeaderMap) -> impl IntoResponse {
    crate::http::static_assets::serve("index.html", headers.get(axum::http::header::IF_NONE_MATCH))
}

/// Static handler for `/static/*`. Path is the remainder after `/static/`.
pub async fn static_path_handler(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    crate::http::static_assets::serve(&path, headers.get(axum::http::header::IF_NONE_MATCH))
}

/// Like `serve_static("index.html")`, but injects a
/// `<script>window.nagentConfig = …</script>` block right before
/// `</head>` so the JS can hide runtime-disabled panels without an
/// extra round trip.
///
/// Only `index.html` is rewritten — every other static asset is
/// served verbatim so the cache fingerprint on
/// `/static/version.txt` does not get poisoned by per-request
/// output.
///
/// We use a non-executable `<script type="application/json">`
/// block (NOT an inline `<script>`) so the page's
/// `Content-Security-Policy: script-src 'self' 'wasm-unsafe-eval'`
/// does not block the injection. A JSON-typed `<script>` block is
/// not executed by the browser — only read via
/// `/healthz` handler.
pub async fn healthz(State(stt): State<SttState>) -> impl IntoResponse {
    if stt.ready.load(std::sync::atomic::Ordering::Acquire) {
        (axum::http::StatusCode::OK, "ok")
    } else {
        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "starting")
    }
}

/// `GET /api/version` — reports the backend crate version and the
/// content hash of the embedded frontend assets.
///
/// The browser reads `/static/version.txt` on page load to learn which
/// frontend bundle it is currently running, then polls this endpoint to
/// detect when the server has been rebuilt with newer assets and the
/// page should be reloaded.
pub async fn version_handler() -> impl IntoResponse {
    let info = VersionInfo::current();
    // Explicit JSON content type so a manual `curl` inspection works
    // without having to chase down axum's default negotiation.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json; charset=utf-8"),
    );
    // Disable caching: a stale "everything is fine" answer would be
    // worse than useless, it would defeat the whole point of the check.
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    let body = serde_json::to_vec(&info)
        .unwrap_or_else(|e| panic!("VersionInfo serialization failed (this is a bug): {e}"));
    (headers, body)
}
