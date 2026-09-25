//! Inference request/response types shared between the WS handler and the
//! inference worker.
//!
//! `InferenceJob` carries everything the worker needs to decode one audio
//! chunk: the originating session ID, the audio samples, the language hint
//! (locked from the session at enqueue time so the worker doesn't need to
//! read session state), and a oneshot channel for the result.

use std::sync::Arc;

use tokio::sync::oneshot;
use uuid::Uuid;

use stt_proto::Segment;

/// One audio chunk waiting to be transcribed.
///
/// `samples` is 16 kHz mono PCM Float32. The expected duration depends on
/// the client's VAD configuration but is typically 1-5 seconds.
#[derive(Debug)]
pub struct InferRequest {
    /// Session this request belongs to. Never trusted from the client.
    pub session_id: Uuid,
    pub samples: Arc<Vec<f32>>,
    /// `None` means "auto-detect".
    pub language: Option<String>,
    /// Translate to English instead of transcribing in source language.
    pub translate: bool,
}

/// Outcome of a single inference call, sent back to the WS handler.
#[derive(Debug)]
pub struct InferResponse {
    pub session_id: Uuid,
    pub text: String,
    pub segments: Vec<Segment>,
    pub lang: String,
    /// Total wall time spent inside the backend, useful for logs/metrics.
    pub duration_ms: u64,
}

/// A unit of work handed to the [`super::worker::InferenceWorker`].
///
/// The worker calls `backend.infer(req)`, then forwards the result through
/// the `response_tx`. The WS handler awaits the oneshot.
#[derive(Debug)]
pub struct InferenceJob {
    pub request: InferRequest,
    pub response_tx: oneshot::Sender<InferResponse>,
}
