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

/// Abstraction over a speech-to-text backend.
///
/// The MVP serializes all calls through a single global `WhisperBackend`;
/// the trait exists so tests can plug a deterministic mock and so the
/// production implementation can be upgraded to a pool later.
#[async_trait]
pub trait WhisperBackend: Send + Sync {
    /// Run inference on a single audio chunk.
    async fn infer(&self, req: InferRequest) -> Result<InferResponse, BackendError>;

    /// Short, human-readable name of the active backend (e.g. `"vulkan"`).
    fn backend_name(&self) -> &'static str;

    /// Identifier of the loaded model (e.g. `"ggml-base.bin"`).
    fn model_id(&self) -> &str;
}
