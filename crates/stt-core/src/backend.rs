//! The [`WhisperBackend`] trait abstracts over the actual transcription
//! engine so the server can be tested without any GPU and so we can later
//! swap in a sticky per-session pool without changing the WS handler.

use async_trait::async_trait;

use crate::job::{InferRequest, InferResponse};

/// Errors that any backend implementation may produce.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("backend not ready: {0}")]
    NotReady(String),
    #[error("inference failed: {0}")]
    Inference(String),
    #[error("backend overloaded")]
    Overloaded,
}

/// Snapshot of the backend's static capabilities, advertised at startup
/// so the server can size its worker pool and so the UI can warn the
/// user about parallelism settings.
///
/// `model_size_bytes` lets the server pick a sensible default worker
/// count even when the operator did not set [`Config::inference_workers`](crate::worker::recommended_worker_count)
/// explicitly — see P2 of the perf plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendInfo {
    /// Short backend label (`"cpu"`, `"vulkan"`, `"cuda"`, `"hipblas"`, `"mock"`).
    pub name: String,
    /// Model filename or identifier (e.g. `"ggml-base.bin"`).
    pub model_id: String,
    /// On-disk size of the loaded model, in bytes. `0` when the backend
    /// is in-process (mock) and did not read anything from disk.
    pub model_size_bytes: u64,
    /// Hint from the backend itself on how many independent contexts it
    /// can sustain safely. `0` means "no opinion — let the server pick".
    pub recommended_workers: usize,
}

impl BackendInfo {
    /// Build a minimal info record. Used by backends that have nothing
    /// meaningful to say about model size or worker count (e.g. mocks).
    pub fn minimal(name: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            model_id: model_id.into(),
            model_size_bytes: 0,
            recommended_workers: 0,
        }
    }

    /// Build a full info record from all known fields.
    pub fn new(
        name: impl Into<String>,
        model_id: impl Into<String>,
        model_size_bytes: u64,
        recommended_workers: usize,
    ) -> Self {
        Self {
            name: name.into(),
            model_id: model_id.into(),
            model_size_bytes,
            recommended_workers,
        }
    }
}

/// Abstraction over a speech-to-text backend.
///
/// The MVP serialized all calls through a single global `WhisperBackend`;
/// the trait is the seam that lets us replace this with a sticky
/// per-session pool without changing the WebSocket layer, and the new
/// [`WhisperBackend::info`] method lets the pool size itself from the
/// loaded model's properties (P2 of the perf plan).
#[async_trait]
pub trait WhisperBackend: Send + Sync {
    /// Run inference on a single audio chunk.
    async fn infer(&self, req: InferRequest) -> Result<InferResponse, BackendError>;

    /// Snapshot of the backend's static capabilities. Called once at
    /// startup; the returned values feed the worker-pool sizing and the
    /// wire-level [`stt_proto::BackendInfo`] advertised to clients.
    fn info(&self) -> BackendInfo;

    /// Short, human-readable name of the active backend (e.g. `"vulkan"`).
    fn backend_name(&self) -> &'static str;

    /// Identifier of the loaded model (e.g. `"ggml-base.bin"`).
    fn model_id(&self) -> &str;
}
