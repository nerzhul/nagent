//! `tts::routes` — HTTP handlers for the TTS subsystem.
//!
//! Two routes live here, both protected by the same LLM-proxy CORS /
//! per-IP rate-limit envelope:
//!
//! - `POST /v1/audio/speech` — synthesise a single chunk of text into
//! 16-bit PCM mono WAV bytes.
//! - `GET /v1/audio/voices` — list the voices discovered on disk plus
//! the per-language defaults.
//!
//! Moved out of the original `tts.rs` as part of the architecture restructuring of the
//! architecture refactor; the public function names (`audio_speech`,
//! `audio_voices`) and the wire types (`SpeechRequest`, `VoicesResponse`,
//! `VoiceMetaJson`) are unchanged.

use axum::extract::{Json, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::tts::engine::{TtsEngine, TtsError, VoiceMeta};
use crate::TtsState;

/// Request body for `POST /v1/audio/speech`. Mirrors the fields the
/// browser sends; everything except `input` is optional.
#[derive(Debug, Deserialize)]
pub struct SpeechRequest {
    /// Text to synthesize. Required. Hard-capped at
    /// `TtsConfig::max_input_chars` (default 2 000).
    pub input: String,
    /// Override the resolved voice id. Wins over `lang`.
    pub voice: Option<String>,
    /// Language hint (`"en"`, `"fr"`, …). Used to pick
    /// `default_voice_en` vs `default_voice_fr` when `voice` is `None`.
    pub lang: Option<String>,
    /// Piper `length_scale` (>1.0 = slower, <1.0 = faster). `None`
    /// keeps the voice's own `.onnx.json` default.
    pub speed: Option<f32>,
}

/// Response body for `GET /v1/audio/voices` — the list of Piper voices
/// discovered in `model_dir`. Returned as JSON so the UI can populate
/// its voice selectors without hard-coding voice ids.
#[derive(Debug, Serialize)]
pub struct VoicesResponse {
    pub voices: Vec<VoiceMetaJson>,
    /// Default voice id used when the request specifies `lang = "en"`
    /// (or any non-French language) without a voice override.
    pub default_voice_en: String,
    /// Default voice id used when the request specifies `lang = "fr"`.
    pub default_voice_fr: String,
    /// Default language code used when the request omits one.
    pub default_lang: String,
}

#[derive(Debug, Serialize)]
pub struct VoiceMetaJson {
    pub id: String,
    pub language: Option<String>,
    pub sample_rate: u32,
}

impl From<&VoiceMeta> for VoiceMetaJson {
    fn from(v: &VoiceMeta) -> Self {
        Self {
            id: v.id.clone(),
            language: v.language.clone(),
            sample_rate: v.sample_rate,
        }
    }
}

/// `POST /v1/audio/speech` — synthesise a single chunk of text and
/// return the WAV bytes (`audio/wav`). The actual Piper call runs on
/// the bounded blocking pool because piper-rs is synchronous and a
/// single short-phrase inference can take 80-200 ms.
pub async fn audio_speech(
    State(tts_state): State<TtsState>,
    Json(req): Json<SpeechRequest>,
) -> Response {
    // Plan R4a: route through the bounded blocking pool. Piper-rs
    // holds a model in memory; the semaphore (default cap = 1)
    // keeps concurrent callers from competing for it.
    let result = tts_state
        .engine
        .synth_wav_bounded(
            &req.input,
            req.voice.as_deref(),
            req.lang.as_deref(),
            req.speed,
        )
        .await;
    match result {
        Ok(wav) => {
            let len = wav.len();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, HeaderValue::from_static("audio/wav")),
                    (
                        header::CONTENT_LENGTH,
                        HeaderValue::from_str(&len.to_string())
                            .unwrap_or(HeaderValue::from_static("0")),
                    ),
                    // Marker so curl --include output makes the route
                    // obvious in logs / smoke tests.
                    (
                        header::HeaderName::from_static("x-tts-backend"),
                        HeaderValue::from_static("piper"),
                    ),
                ],
                wav,
            )
                .into_response()
        }
        Err(TtsError::EmptyInput) => (
            StatusCode::BAD_REQUEST,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            "input is empty",
        )
            .into_response(),
        Err(TtsError::InputTooLong(got, max)) => (
            StatusCode::BAD_REQUEST,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            format!("input too long: {got} chars (max {max})"),
        )
            .into_response(),
        Err(TtsError::VoiceNotFound(name)) => (
            StatusCode::NOT_FOUND,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            format!("voice not found: {name}"),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            format!("TTS failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /v1/audio/voices` — list the Piper voices discovered in
/// `model_dir`, plus the per-language defaults. Used by the discussion
/// UI's voice selectors so they only ever offer voices that actually
/// exist on disk.
pub async fn audio_voices(State(tts_state): State<TtsState>) -> Response {
    let tts = &tts_state.engine;
    let body = VoicesResponse {
        voices: tts.voices().iter().map(VoiceMetaJson::from).collect(),
        default_voice_en: tts.default_voice_for("en"),
        default_voice_fr: tts.default_voice_for("fr"),
        default_lang: tts.default_lang().to_string(),
    };
    (StatusCode::OK, Json(body)).into_response()
}

// Reference the engine type so doc-comments referencing
// `TtsEngine::synth_wav` (which now lives in `engine.rs`) still
// resolve for downstream doc generation.
#[allow(dead_code)]
fn _engine_doc_link(_: &TtsEngine) {}
