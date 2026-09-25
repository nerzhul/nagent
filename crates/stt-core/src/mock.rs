//! `MockBackend` — in-process `WhisperBackend` used for tests and for
//! development/CI when the real whisper-rs backend is not available.
//!
//! The mock echoes back the request's `session_id` and a fixed text, which
//! lets the integration test assert routing correctness without any GPU.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::backend::{BackendError, WhisperBackend};
use crate::job::{InferRequest, InferResponse};

/// Always-available mock backend used by default.
#[derive(Debug, Clone)]
pub struct MockBackend {
    model_id: String,
}

impl MockBackend {
    /// Build a mock that reports `model_id` as its loaded model.
    pub fn new(model_id: impl Into<String>) -> Self {
        Self {
            model_id: model_id.into(),
        }
    }
}

#[async_trait]
impl WhisperBackend for MockBackend {
    async fn infer(&self, req: InferRequest) -> Result<InferResponse, BackendError> {
        // The echoed text includes the session ID, which lets tests assert
        // that the ResultRouter sent the right response to the right
        // session.
        Ok(InferResponse {
            session_id: req.session_id,
            text: format!("mock-session={}", req.session_id),
            segments: vec![stt_proto::Segment {
                text: format!("mock-session={}", req.session_id),
                t0_ms: 0,
                t1_ms: (req.samples.len() as f32 / 16.0) as u32,
                no_speech_prob: 0.0,
            }],
            lang: req.language.unwrap_or_else(|| "en".into()),
            duration_ms: 0,
        })
    }

    fn backend_name(&self) -> &'static str {
        "mock"
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// Trait helper to convert into an `Arc<dyn WhisperBackend>`.
pub fn into_arc(b: MockBackend) -> Arc<dyn WhisperBackend> {
    Arc::new(b)
}

// Re-export for convenience.
pub use into_arc as into_shared;

// Silence "unused" warnings when only some fields are referenced.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<Uuid>();
}
