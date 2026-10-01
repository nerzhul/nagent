//! Local text-to-speech (Piper ONNX) subsystem.
//!
//! Plan 4.F moved the engine itself (`Synthesizer`, `TtsEngine`,
//! `MockSynthesizer`, `PiperSynthesizer`, `TtsError`, `VoiceMeta`)
//! into the `nagent-tts` crate so the second heavy native runtime
//! can be split into its own inference tier later (plan H). The
//! HTTP routes stay here because they depend on axum and the
//! server's state type.
//!
//! The public re-exports below keep the old `crate::tts::TtsEngine`
//! call sites compiling until the routes are migrated in place.

pub mod routes;

// Re-export the engine surface so existing call sites (routes,
// `TtsState`, the unit tests) keep working. New code should reach
// for `nagent_tts::*` directly.
pub use nagent_tts::{
    MockSynthesizer, SynthOutput, Synthesizer, TtsEngine, TtsError, TtsSettings, VoiceMeta,
};
pub use routes::{audio_speech, audio_voices, SpeechRequest, VoiceMetaJson, VoicesResponse};
