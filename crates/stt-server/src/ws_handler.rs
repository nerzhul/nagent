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
use axum::http::StatusCode;
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use std::net::{IpAddr, SocketAddr};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use stt_core::{InferRequest, InferenceJob};
use stt_proto::{decode_frame, encode_frame, BackendInfo, ErrorMessage, Payload};

use crate::rate_limit::RateLimitError;
use crate::router::send_to_session;
use crate::session::{register, unregister, OutboundMessage};
use crate::static_assets::{mime_for, StaticAssets};
use crate::validation::FrameError;
use crate::version::VersionInfo;
use crate::AppState;

/// Handle a WebSocket upgrade request.
///
/// One token is consumed from the STT rate limiter keyed by the peer
/// IP. A bucket miss is reported as `429 Too Many Requests` on the
/// HTTP upgrade handshake rather than as a WS close, so the operator
/// can see the rejection in plain HTTP logs without having to decode
/// the close frame.
pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    peer: Option<ConnectInfo<SocketAddr>>,
) -> impl IntoResponse {
    let peer_ip = peer.map(|ConnectInfo(addr)| addr.ip());
    let limiter = state.stt_rate_limiter.clone();
    let state_for_conn = Arc::clone(&state);

    // Decide whether to upgrade before consuming the upgrade itself;
    // this way a rejection is a plain 429 (no WS handshake started).
    match peer_ip {
        Some(ip) => match limiter.check(ip) {
            Ok(()) => ws
                .on_upgrade(move |socket| ws_connection(socket, state_for_conn, Some(ip)))
                .into_response(),
            Err(RateLimitError::Limited { retry_after_ms, .. }) => {
                let secs = retry_after_ms.div_ceil(1000).max(1);
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
        None => ws
            .on_upgrade(move |socket| ws_connection(socket, state_for_conn, None))
            .into_response(),
    }
}

/// Per-connection task: loops between inbound frames and outbound messages.
///
/// `peer_ip`, when present, is used by the per-frame rate-limit check
/// to debounce flooding clients. When absent (in-process test harness
/// without a real TCP listener), per-frame checks are skipped — the
/// HTTP upgrade gate still applied whenever a real peer address was
/// available.
pub async fn ws_connection(socket: WebSocket, app: Arc<AppState>, peer_ip: Option<IpAddr>) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let limiter = app.stt_rate_limiter.clone();

    // Register BEFORE any send so we know our session ID and the outbound
    // channel is owned exclusively by this task.
    let (session_id, _state, mut outbound_rx) = register(&app.sessions, 64);

    info!(%session_id, "ws connection opened");

    // Send BackendInfo immediately so the client knows which model/GPU is in use.
    let info = BackendInfo {
        model_id: app.backend.model_id().to_string(),
        gpu_backend: app.backend.backend_name().to_string(),
    };
    match encode_frame(&Payload::Backend(info)) {
        Ok(bytes) => {
            if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                warn!(%session_id, "failed to send BackendInfo, closing");
                unregister(&app.sessions, session_id);
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
                            warn!(%session_id, "ws inbound rate-limited: {e}");
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
                        if let Err(e) = handle_inbound(&app, session_id, &buf).await {
                            warn!(%session_id, "inbound error: {e}");
                            if matches!(e, InboundError::ClientStop) {
                                break;
                            }
                            let err_payload = Payload::Error(ErrorMessage {
                                code: 1,
                                message: format!("{e}"),
                            });
                            if let Ok(bytes) = encode_frame(&err_payload) {
                                if ws_tx.send(Message::Binary(bytes)).await.is_err() {
                                    warn!(%session_id, "ws send error frame failed");
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
                        warn!(%session_id, "unexpected text frame");
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
    // results are dropped instead of being routed into a dead channel.
    unregister(&app.sessions, session_id);
    info!(%session_id, "ws connection closed");
}

/// Decode an inbound frame and dispatch it.
async fn handle_inbound(
    app: &Arc<AppState>,
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
                &app.config.limits,
                start.sample_rate,
                start.lang_hint.as_deref(),
            )
            .map_err(InboundError::Validation)?;
            // Snapshot the Arc out of the DashMap shard before any await.
            let state_arc = app.sessions.get(&session_id).map(|s| s.value().clone());
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
            FrameError::validate_config(&app.config.limits, cfg.language.as_deref())
                .map_err(InboundError::Validation)?;
            let state_arc = app.sessions.get(&session_id).map(|s| s.value().clone());
            if let Some(state) = state_arc {
                state.touch();
                state.set_language(normalize_lang_hint(cfg.language));
                state.set_translate(cfg.translate);
            }
        }
        Payload::Audio(audio) => {
            FrameError::validate_audio(&app.config.limits, &audio.samples)
                .map_err(InboundError::Validation)?;
            // Snapshot config from the session before we send the job;
            // never hold a DashMap shard lock across an .await.
            let snapshot = app.sessions.get(&session_id).map(|s| s.value().clone());
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
            // If the queue is full, the send fails — surface an error to the client.
            app.job_tx
                .send(job)
                .await
                .map_err(|_| InboundError::WorkerUnavailable)?;

            // Spawn a small task that awaits the result and pushes it through
            // the result router (which looks up the session by ID).
            let map = Arc::clone(&app.sessions);
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

/// Static handler for `/` and `/index.html`.
pub async fn index_handler() -> impl IntoResponse {
    serve_static("index.html")
}

/// Static handler for `/static/*`. Path is the remainder after `/static/`.
pub async fn static_path_handler(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl IntoResponse {
    serve_static(&path)
}

fn serve_static(path: &str) -> (axum::http::HeaderMap, Vec<u8>) {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static(mime_for(path)),
    );
    let bytes = StaticAssets::get(path)
        .map(|f| f.data.into_owned())
        .unwrap_or_default();
    (headers, bytes)
}

/// `/healthz` handler.
pub async fn healthz(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if state.ready.load(std::sync::atomic::Ordering::Acquire) {
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
