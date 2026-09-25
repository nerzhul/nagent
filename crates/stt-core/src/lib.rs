//! `stt-core` — backend-agnostic inference engine for the STT pipeline.
//!
//! The crate is built around a single trait, [`WhisperBackend`], that the
//! server talks to. The MVP ships a serialized worker backed by a single
//! context, but the trait is the seam that lets us upgrade to a sticky
//! per-session pool later without touching the WebSocket layer.
//!
//! ```text
//! AudioFrame ──► InferenceJob ──► mpsc ──► InferenceWorker ──► backend.infer()
//!                                                                      │
//!                                                  InferResponse (oneshot)
//! ```

#![warn(missing_debug_implementations)]

pub mod backend;
pub mod job;
pub mod mock;
pub mod worker;

#[cfg(feature = "whisper-rs-backend")]
pub mod whisper_backend;

pub use backend::WhisperBackend;
pub use job::{InferRequest, InferResponse, InferenceJob};
pub use mock::MockBackend;
pub use worker::{InferenceWorker, WorkerHandle};
