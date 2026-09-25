//! Production [`WhisperBackend`] implementation backed by `whisper-rs`.
//!
//! The MVP serializes all inference through a single `WhisperContext` +
//! `WhisperState`, guarded by a `Mutex`. The trait abstraction lets us
//! replace this with a sticky per-session pool later without touching the
//! WS layer.

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tracing::{info, warn};
use uuid::Uuid;

use stt_proto::Segment;

use crate::backend::{BackendError, WhisperBackend};
use crate::job::{InferRequest, InferResponse};

/// Production backend wrapping a single `WhisperContext` + `WhisperState`.
///
/// Inference is serialized internally. Callers should still wrap this in
/// `Arc<dyn WhisperBackend>` and feed it to an [`crate::InferenceWorker`].
pub struct WhisperRsBackend {
    state: Arc<Mutex<whisper_rs::WhisperState>>,
    model_id: String,
    backend_name: &'static str,
}

impl WhisperRsBackend {
    /// Load a ggml-format model from disk and warm the context up.
    pub fn load(model_path: impl AsRef<Path>) -> Result<Self, BackendError> {
        let model_path = model_path.as_ref();
        let model_id = model_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("model")
            .to_string();

        let params = whisper_rs::WhisperContextParameters::default();
        let ctx = whisper_rs::WhisperContext::new_with_params(model_path, params)
            .map_err(|e| BackendError::NotReady(format!("load model: {e}")))?;

        let state = ctx
            .create_state()
            .map_err(|e| BackendError::NotReady(format!("create state: {e}")))?;

        info!(model = %model_id, "whisper model loaded");

        Ok(Self {
            state: Arc::new(Mutex::new(state)),
            model_id,
            backend_name: detect_backend_name(),
        })
    }

    /// Build [`FullParams`] for the given request.
    fn build_params(req: &InferRequest) -> whisper_rs::FullParams<'static, 'static> {
        // Greedy = lowest latency. Translation is opt-in from the client.
        let mut params = whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_single_segment(false);
        params.set_max_len(0);
        params.set_translate(req.translate);

        if let Some(lang) = req.language.as_deref() {
            // Setting an explicit language hint skips auto-detection latency.
            params.set_language(Some(lang));
        }

        params
    }
}

#[async_trait]
impl WhisperBackend for WhisperRsBackend {
    async fn infer(&self, req: InferRequest) -> Result<InferResponse, BackendError> {
        let params = Self::build_params(&req);
        let session_id: Uuid = req.session_id;
        let samples = req.samples.clone();
        let state = Arc::clone(&self.state);

        // The blocking inference call is moved off the async runtime thread
        // via spawn_blocking so we don't stall other Tokio tasks.
        tokio::task::spawn_blocking(move || -> Result<InferResponse, BackendError> {
            let mut guard = state.lock().expect("whisper mutex poisoned");
            guard
                .full(params, samples.as_slice())
                .map_err(|e| BackendError::Inference(e.to_string()))?;

            let n = guard.full_n_segments();
            let mut segments = Vec::with_capacity(n as usize);
            let mut full_text = String::new();
            for i in 0..n {
                let seg = guard
                    .get_segment(i)
                    .ok_or_else(|| BackendError::Inference(format!("missing segment {i}")))?;
                let text = seg.text().to_string();
                full_text.push_str(&text);
                segments.push(Segment {
                    text,
                    t0_ms: seg.start_timestamp() as u32,
                    t1_ms: seg.end_timestamp() as u32,
                    no_speech_prob: seg.no_speech_probability(),
                });
            }

            let lang_id = guard.full_lang_id_from_state();
            let lang = whisper_rs::get_lang_str(lang_id)
                .unwrap_or("auto")
                .to_string();

            Ok(InferResponse {
                session_id,
                text: full_text.trim().to_string(),
                segments,
                lang,
                duration_ms: 0,
            })
        })
        .await
        .map_err(|e| BackendError::Inference(format!("join error: {e}")))?
    }

    fn backend_name(&self) -> &'static str {
        self.backend_name
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }
}

/// Pick a backend label from compile-time features, in priority order:
/// Vulkan > CUDA > HIP > CPU.
fn detect_backend_name() -> &'static str {
    #[cfg(feature = "vulkan")]
    {
        return "vulkan";
    }
    #[cfg(not(feature = "vulkan"))]
    #[cfg(feature = "cuda")]
    {
        return "cuda";
    }
    #[cfg(not(any(feature = "vulkan", feature = "cuda")))]
    #[cfg(feature = "hipblas")]
    {
        return "hipblas";
    }
    #[cfg(not(any(feature = "vulkan", feature = "cuda", feature = "hipblas")))]
    {
        warn!("no GPU backend feature enabled; whisper will run on CPU");
        "cpu"
    }
}
