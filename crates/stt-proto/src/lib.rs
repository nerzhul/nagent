//! Wire types and `postcard` codec for the STT WebSocket protocol.
//!
//! Every WebSocket frame carries a single byte tag followed by a `postcard`-
//! serialized payload. Tags are listed in [`Tag`].

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

use serde::{Deserialize, Serialize};

/// Identifies the payload that follows a single byte at the start of a frame.
///
/// Tags are explicit and stable: changing one is a wire-protocol break.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tag {
    /// PCM Float32 mono audio chunk, posted by the client.
    AudioFrame = 0x01,
    /// Client asks the server to begin a session.
    StartSession = 0x10,
    /// Client asks the server to end the current session.
    StopSession = 0x11,
    /// Client changes runtime configuration (language, translate).
    Config = 0x12,
    /// Server emits an incremental transcript while still decoding.
    PartialTranscript = 0x20,
    /// Server emits a final transcript after decoding finishes.
    FinalTranscript = 0x21,
    /// Server reports a protocol or runtime error.
    Error = 0x30,
    /// Server advertises which model + GPU backend is in use.
    BackendInfo = 0x31,
}

impl Tag {
    /// Try to convert a raw byte into a [`Tag`].
    pub fn from_u8(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(Self::AudioFrame),
            0x10 => Some(Self::StartSession),
            0x11 => Some(Self::StopSession),
            0x12 => Some(Self::Config),
            0x20 => Some(Self::PartialTranscript),
            0x21 => Some(Self::FinalTranscript),
            0x30 => Some(Self::Error),
            0x31 => Some(Self::BackendInfo),
            _ => None,
        }
    }
}

/// Single audio segment emitted by the client after client-side VAD.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioFrame {
    /// Mono PCM Float32 LE samples, 16 kHz. Length is `n_samples`.
    pub samples: Vec<f32>,
}

/// Handshake from client to start a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StartSession {
    /// Optional ISO 639-1 language hint, e.g. `"en"`, `"fr"`.
    pub lang_hint: Option<String>,
    /// Sample rate the client downsampled to (must be 16000 for whisper).
    pub sample_rate: u32,
}

/// Empty payload that ends the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopSession;

/// Runtime configuration update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Override language (`None` = auto-detect).
    pub language: Option<String>,
    /// Translate to English instead of transcribing in the source language.
    pub translate: bool,
}

/// One decoded segment within a [`FinalTranscript`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    pub t0_ms: u32,
    pub t1_ms: u32,
    pub no_speech_prob: f32,
}

/// Incremental, non-final transcript emitted while decoding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PartialTranscript {
    pub text: String,
    pub t0_ms: u32,
    pub t1_ms: u32,
    pub lang: String,
}

/// Final transcript for a fully decoded audio chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalTranscript {
    pub text: String,
    pub segments: Vec<Segment>,
    pub lang: String,
}

/// Server- or client-side error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorMessage {
    /// Stable error code, see [`ErrorCode`].
    pub code: u16,
    pub message: String,
}

/// Stable numeric error codes.
pub mod error_code {
    pub const UNKNOWN: u16 = 0;
    pub const INVALID_FRAME: u16 = 1;
    pub const INTERNAL: u16 = 2;
    pub const BACKEND_UNAVAILABLE: u16 = 3;
    pub const QUEUE_FULL: u16 = 4;
    pub const MODEL_NOT_LOADED: u16 = 5;
}

/// Server advertises which model and GPU backend it picked at startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    /// Model filename, e.g. `"ggml-base.bin"`.
    pub model_id: String,
    /// Human-readable backend name, e.g. `"vulkan"`, `"cuda"`, `"hipblas"`, `"cpu"`.
    pub gpu_backend: String,
}

/// Errors produced by the codec.
#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame is empty")]
    EmptyFrame,
    #[error("unknown tag byte 0x{0:02x}")]
    UnknownTag(u8),
    #[error("postcard decode failed: {0}")]
    Postcard(#[from] postcard::Error),
}

/// All payload types that can travel over the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Payload {
    Audio(AudioFrame),
    Start(StartSession),
    Stop(StopSession),
    Config(Config),
    Partial(PartialTranscript),
    Final(FinalTranscript),
    Error(ErrorMessage),
    Backend(BackendInfo),
}

impl Payload {
    /// The tag byte that prefixes this payload on the wire.
    pub fn tag(&self) -> Tag {
        match self {
            Self::Audio(_) => Tag::AudioFrame,
            Self::Start(_) => Tag::StartSession,
            Self::Stop(_) => Tag::StopSession,
            Self::Config(_) => Tag::Config,
            Self::Partial(_) => Tag::PartialTranscript,
            Self::Final(_) => Tag::FinalTranscript,
            Self::Error(_) => Tag::Error,
            Self::Backend(_) => Tag::BackendInfo,
        }
    }

    /// Deserialize the trailing bytes of a frame into the variant struct
    /// selected by `tag`.
    fn from_tagged(tag: Tag, tail: &[u8]) -> Result<Self, CodecError> {
        let payload = match tag {
            Tag::AudioFrame => Self::Audio(postcard::from_bytes(tail)?),
            Tag::StartSession => Self::Start(postcard::from_bytes(tail)?),
            Tag::StopSession => Self::Stop(postcard::from_bytes(tail)?),
            Tag::Config => Self::Config(postcard::from_bytes(tail)?),
            Tag::PartialTranscript => Self::Partial(postcard::from_bytes(tail)?),
            Tag::FinalTranscript => Self::Final(postcard::from_bytes(tail)?),
            Tag::Error => Self::Error(postcard::from_bytes(tail)?),
            Tag::BackendInfo => Self::Backend(postcard::from_bytes(tail)?),
        };
        Ok(payload)
    }
}

/// Encode a payload into a single WebSocket binary frame.
///
/// The output is `vec![tag, ..postcard_bytes]`.
///
/// The byte tag is the **only** type discriminator on the wire: the inner
/// payload is serialized as the matching variant struct (e.g.
/// `AudioFrame`), not as the wrapping [`Payload`] enum. Encoding the enum
/// would make postcard prepend a variant-index varint, which would shift
/// every field in clients that only expect `[tag, payload]` (e.g. the JS
/// frontend).
pub fn encode_frame(payload: &Payload) -> Result<Vec<u8>, CodecError> {
    let tag = payload.tag() as u8;
    let mut buf = match payload {
        Payload::Audio(p) => postcard::to_allocvec(p)?,
        Payload::Start(p) => postcard::to_allocvec(p)?,
        Payload::Stop(p) => postcard::to_allocvec(p)?,
        Payload::Config(p) => postcard::to_allocvec(p)?,
        Payload::Partial(p) => postcard::to_allocvec(p)?,
        Payload::Final(p) => postcard::to_allocvec(p)?,
        Payload::Error(p) => postcard::to_allocvec(p)?,
        Payload::Backend(p) => postcard::to_allocvec(p)?,
    };
    buf.insert(0, tag);
    Ok(buf)
}

/// Convenience wrapper: encode a specific payload variant.
pub fn encode<T: Into<Payload>>(payload: T) -> Result<Vec<u8>, CodecError> {
    encode_frame(&payload.into())
}

/// Decode a WebSocket binary frame back into its tag and payload.
///
/// The byte tag selects which inner struct is deserialized from the
/// remainder of the frame. See [`encode_frame`] for why the [`Payload`]
/// enum itself is never on the wire.
pub fn decode_frame(bytes: &[u8]) -> Result<Payload, CodecError> {
    let (head, tail) = bytes.split_first().ok_or(CodecError::EmptyFrame)?;
    let tag = Tag::from_u8(*head).ok_or(CodecError::UnknownTag(*head))?;
    Ok(Payload::from_tagged(tag, tail)?)
}

// --- From impls so callers can `encode(audio_frame)` without the enum. -------

impl From<AudioFrame> for Payload {
    fn from(v: AudioFrame) -> Self {
        Self::Audio(v)
    }
}
impl From<StartSession> for Payload {
    fn from(v: StartSession) -> Self {
        Self::Start(v)
    }
}
impl From<StopSession> for Payload {
    fn from(v: StopSession) -> Self {
        Self::Stop(v)
    }
}
impl From<Config> for Payload {
    fn from(v: Config) -> Self {
        Self::Config(v)
    }
}
impl From<PartialTranscript> for Payload {
    fn from(v: PartialTranscript) -> Self {
        Self::Partial(v)
    }
}
impl From<FinalTranscript> for Payload {
    fn from(v: FinalTranscript) -> Self {
        Self::Final(v)
    }
}
impl From<ErrorMessage> for Payload {
    fn from(v: ErrorMessage) -> Self {
        Self::Error(v)
    }
}
impl From<BackendInfo> for Payload {
    fn from(v: BackendInfo) -> Self {
        Self::Backend(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_round_trip() {
        for b in 0u8..=255 {
            if let Some(tag) = Tag::from_u8(b) {
                assert_eq!(tag as u8, b);
            }
        }
    }

    #[test]
    fn unknown_tag_is_rejected() {
        for bad in [0x00u8, 0x02, 0x0F, 0x1F, 0x22, 0x2F, 0x32, 0xFF] {
            assert!(
                Tag::from_u8(bad).is_none(),
                "byte {bad:#x} should be unknown"
            );
        }
    }

    #[test]
    fn audio_frame_round_trip() {
        let frame = AudioFrame {
            samples: vec![0.0, 1.0, -1.0, 0.5, -0.5],
        };
        let bytes = encode(Payload::Audio(frame.clone())).unwrap();
        assert_eq!(bytes[0], Tag::AudioFrame as u8);
        let decoded = decode_frame(&bytes).unwrap();
        assert_eq!(decoded, Payload::Audio(frame));
    }

    #[test]
    fn start_session_round_trip() {
        let s = StartSession {
            lang_hint: Some("fr".into()),
            sample_rate: 16000,
        };
        let bytes = encode(Payload::Start(s.clone())).unwrap();
        let decoded = decode_frame(&bytes).unwrap();
        assert_eq!(decoded, Payload::Start(s));
    }

    #[test]
    fn config_round_trip() {
        let c = Config {
            language: Some("en".into()),
            translate: true,
        };
        let bytes = encode(Payload::Config(c.clone())).unwrap();
        let decoded = decode_frame(&bytes).unwrap();
        assert_eq!(decoded, Payload::Config(c));
    }

    #[test]
    fn final_transcript_round_trip() {
        let t = FinalTranscript {
            text: "hello world".into(),
            segments: vec![Segment {
                text: "hello world".into(),
                t0_ms: 0,
                t1_ms: 1500,
                no_speech_prob: 0.01,
            }],
            lang: "en".into(),
        };
        let bytes = encode(Payload::Final(t.clone())).unwrap();
        let decoded = decode_frame(&bytes).unwrap();
        assert_eq!(decoded, Payload::Final(t));
    }

    #[test]
    fn empty_frame_is_rejected() {
        let err = decode_frame(&[]).unwrap_err();
        assert!(matches!(err, CodecError::EmptyFrame));
    }

    #[test]
    fn unknown_tag_byte_is_rejected() {
        let err = decode_frame(&[0xFE, 0x00]).unwrap_err();
        assert!(matches!(err, CodecError::UnknownTag(0xFE)));
    }

    #[test]
    fn payload_tag_matches_variant() {
        let p = Payload::Stop(StopSession);
        assert_eq!(p.tag() as u8, 0x11);
    }

    /// The wire format MUST be `[custom_tag, inner_payload]`. In particular,
    /// `encode_frame` must not prepend the `Payload` enum variant index,
    /// because that would shift every field on the receiving side (the JS
    /// frontend does not expect it).
    #[test]
    fn wire_format_is_tag_then_inner_only() {
        // AudioFrame { samples: [] } → [0x01, varint(0)]
        let audio = Payload::Audio(AudioFrame { samples: vec![] });
        let bytes = encode_frame(&audio).unwrap();
        assert_eq!(bytes, vec![Tag::AudioFrame as u8, 0x00]);

        // StartSession { lang_hint: None, sample_rate: 16000 }
        //   = [0x10, 0x00 (Option::None), 0x80, 0x7D (varint 16000)]
        let start = Payload::Start(StartSession {
            lang_hint: None,
            sample_rate: 16000,
        });
        let bytes = encode_frame(&start).unwrap();
        assert_eq!(bytes, vec![Tag::StartSession as u8, 0x00, 0x80, 0x7D]);

        // BackendInfo { model_id: "a", gpu_backend: "b" }
        //   = [0x31, 0x01 'a', 0x01 'b']
        let backend = Payload::Backend(BackendInfo {
            model_id: "a".into(),
            gpu_backend: "b".into(),
        });
        let bytes = encode_frame(&backend).unwrap();
        assert_eq!(bytes, vec![Tag::BackendInfo as u8, 0x01, b'a', 0x01, b'b']);

        // Error { code: 1, message: "x" }
        //   = [0x30, 0x01 (varint code=1), 0x01 'x']
        let err = Payload::Error(ErrorMessage {
            code: 1,
            message: "x".into(),
        });
        let bytes = encode_frame(&err).unwrap();
        assert_eq!(bytes, vec![Tag::Error as u8, 0x01, 0x01, b'x']);

        // StopSession (unit struct) → just the tag byte.
        let bytes = encode_frame(&Payload::Stop(StopSession)).unwrap();
        assert_eq!(bytes, vec![Tag::StopSession as u8]);
    }

    /// Decoding a frame that starts with an unknown tag must reject the
    /// tag byte **before** touching the trailing bytes — i.e. the error
    /// must come from `Tag::from_u8`, not from postcard.
    #[test]
    fn decode_rejects_unknown_tag_without_consuming_payload() {
        let err = decode_frame(&[0xAA, 0xFF, 0xFF, 0xFF]).unwrap_err();
        match err {
            CodecError::UnknownTag(0xAA) => {}
            other => panic!("expected UnknownTag(0xAA), got {other:?}"),
        }
    }
}
