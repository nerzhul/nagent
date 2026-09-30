//! Local text-to-speech (Piper ONNX) subsystem.
//!
//! - [`engine`] — the [`Synthesizer`] trait (mock + Piper backends),
//!   the [`TtsEngine`](engine::TtsEngine) facade, and the 16-bit PCM
//!   mono WAV encoder.
//! - [`routes`] — the HTTP handlers `POST /v1/audio/speech` and
//!   `GET /v1/audio/voices`.
//!
//! Phase 1 of the architecture refactor split the original
//! `tts.rs` into these two files; the public API is unchanged.

pub mod engine;
pub mod routes;

// Back-compat re-exports — the pre-split callers reached these items
// through `crate::tts::*`; keep that working until the follow-up
// commit rewrites the call sites in place.
pub use engine::{MockSynthesizer, Synthesizer, TtsEngine, TtsError, VoiceMeta};
pub use routes::{audio_speech, audio_voices, SpeechRequest, VoiceMetaJson, VoicesResponse};
