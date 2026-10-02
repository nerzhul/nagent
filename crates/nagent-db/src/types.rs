//! Row types shared between `nagent-db` and the auth / HTTP layers.
//!
//! These types used to live in `stt-server::auth::store` and
//! `stt-server::auth::session`. They now live with the repositories
//! that own them so `nagent-db` does not have to depend on the
//! server crate.
//!
//! The server crate re-exports them from this module so existing
//! call sites (`crate::auth::store::AuthUserRecord`,
//! `crate::auth::session::SessionRecord`) keep resolving.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// ---- Session primitives --------------------------------------------------

/// Length (in bytes) of the random session token. 32 bytes = 256
/// bits of entropy, comfortably above the OWASP 2025 guidance
/// for session identifiers.
pub const SESSION_TOKEN_BYTES: usize = 32;

/// Length (in bytes) of the SHA-256 hash that is persisted in the
/// `sessions.token_hash` column. Always equal to the SHA-256
/// output size.
pub const SESSION_HASH_BYTES: usize = 32;

/// SHA-256 hash of a session token. Used as the primary key in
/// `sessions.token_hash`. Held in stack-allocated arrays so the
/// hot path (every authenticated request) never touches the heap.
pub type SessionTokenHash = [u8; SESSION_HASH_BYTES];

/// A row from the `sessions` table. Internal — handlers never see
/// the raw row, they get a higher-level auth-user struct.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    /// SHA-256 hash of the opaque token (security plan #7).
    /// Stored in `sessions.token_hash` and used as the primary
    /// key. The plaintext token is never persisted and never
    /// reflected back from a `lookup_*` — it is only returned at
    /// creation time via the `plaintext_token` field, which the
    /// caller is expected to consume immediately.
    pub token_hash: SessionTokenHash,
    pub user_id: Uuid,
    pub csrf_token: String,
    pub expires_at: DateTime<Utc>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    /// Plaintext session token — set ONLY at the moment of creation.
    /// Reads leave this as `None`.
    pub plaintext_token: Option<String>,
}

impl SessionRecord {
    /// Move the plaintext session token out of the record.
    pub fn take_plaintext_token(&mut self) -> Option<String> {
        self.plaintext_token.take()
    }
}

/// Generate a new opaque session token. Returns the 32-byte OS-RNG
/// token (base64url-no-pad) plus the SHA-256 hash that is persisted
/// in `sessions.token_hash`.
pub fn new_session_token() -> (String, SessionTokenHash) {
    use rand::RngCore;
    let mut bytes = [0u8; SESSION_TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let hash = sha256_of(&bytes);
    let encoded = URL_SAFE_NO_PAD.encode(bytes);
    (encoded, hash)
}

/// SHA-256 of `bytes`.
pub fn sha256_of(bytes: &[u8]) -> SessionTokenHash {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut hash = [0u8; SESSION_HASH_BYTES];
    hash.copy_from_slice(&out);
    hash
}

/// Base64url-no-pad-decode the wire-format session token into the
/// raw 32 bytes the hash is computed over.
pub fn decode_session_token(token: &str) -> Option<[u8; SESSION_TOKEN_BYTES]> {
    let raw = URL_SAFE_NO_PAD.decode(token).ok()?;
    if raw.len() != SESSION_TOKEN_BYTES {
        return None;
    }
    let mut out = [0u8; SESSION_TOKEN_BYTES];
    out.copy_from_slice(&raw);
    Some(out)
}

/// Generate a CSRF token: 32 bytes of OS RNG, hex-encoded.
pub fn new_csrf_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

// ---- Row types owned by the repositories --------------------------------

/// Public-facing user record. Mirrors the row in the `users` table
/// minus the password hash (which stays inside the store).
#[derive(Debug, Clone)]
pub struct AuthUserRecord {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub provider: String,
    pub created_at: DateTime<Utc>,
    /// `password_hash` is exposed only to the password backend —
    /// other callers should not see it (defence in depth).
    pub password_hash: Option<Vec<u8>>,
}

/// Parameters for `passkeys::insert_passkey`.
#[derive(Debug, Clone)]
pub struct NewPasskeyRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub credential_id: Vec<u8>,
    pub public_key: Vec<u8>,
    pub counter: u32,
    pub transports: String,
    pub aaguid: Option<Vec<u8>>,
}

/// Passkey row returned by `passkeys::get_by_credential_id`.
#[derive(Debug, Clone)]
pub struct PasskeyRecord {
    pub id: Uuid,
    pub user_id: Uuid,
    pub credential_id: Vec<u8>,
    pub public_key: Vec<u8>,
    pub counter: u32,
    pub transports: String,
}

/// Parameters for `events::record`.
#[derive(Debug, Clone, Default)]
pub struct NewAuthEvent {
    pub user_id: Option<Uuid>,
    pub kind: String,
    pub provider: String,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub target_service: Option<String>,
}

impl NewAuthEvent {
    /// Build a non-credential audit row.
    pub fn auth(
        user_id: Option<Uuid>,
        kind: impl Into<String>,
        provider: impl Into<String>,
    ) -> Self {
        Self {
            user_id,
            kind: kind.into(),
            provider: provider.into(),
            ..Default::default()
        }
    }
}

/// One row from the `user_credentials` table.
#[derive(Debug, Clone)]
pub struct UserCredentialRow {
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
}

/// Per-user UI preferences.
#[derive(Debug, Clone)]
pub struct UserPreferences {
    pub share_location_enabled: bool,
    pub share_timezone_enabled: bool,
    /// Language the assistant should reply in (`None` = "Auto", i.e.
    /// match the user's input language). Drives both the LLM
    /// system-message injection in `llm::proxy::chat_completions`
    /// (server-side, via the `USER_REPLY_LANGUAGE_MARKER` block) and
    /// the TTS voice selection in `chat.js::resolveTtsVoice`
    /// (browser-side). Stored as a BCP-47 primary subtag (e.g.
    /// `"fr"`, `"en"`, `"es"`) so the supported list can grow without
    /// another migration.
    pub reply_language: Option<String>,
    pub updated_at: DateTime<Utc>,
}

// ---- Document row -------------------------------------------------------

/// Subset of the `uploaded_documents` row the agent needs. Keeps
/// the DB layer's UUID / RFC 3339 string handling out of the
/// higher-level agent module.
#[derive(Debug, Clone)]
pub struct DocumentRow {
    pub id: Uuid,
    pub original_name: String,
    pub mime: String,
    pub size_bytes: u64,
    pub extracted_chars: u64,
    pub page_count: Option<u32>,
    pub disk_path: std::path::PathBuf,
}

// ---- AuthUser (request identity) ---------------------------------------

/// Public-facing identity attached to every authenticated request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: Uuid,
    pub email: String,
    pub display_name: String,
    pub provider: String,
    pub roles: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub csrf_token: String,
    pub session_expires_at: DateTime<Utc>,
    #[serde(skip_serializing)]
    pub session_token_hash: SessionTokenHash,
    pub session_source: SessionSource,
}

/// Origin of the credential that authenticated a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionSource {
    /// Authenticated via the session cookie. CSRF must be enforced
    /// on state-changing verbs.
    Cookie,
    /// Authenticated via the `Authorization: Bearer …` header.
    /// CSRF does not apply.
    Bearer,
}

// ---- Migration primitives ---------------------------------------------

/// One row of the migration status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRow {
    pub version: i64,
    pub description: String,
}

/// Snapshot of the migration state at one point in time.
#[derive(Debug, Clone, Default)]
pub struct MigrationStatus {
    pub applied: Vec<MigrationRow>,
    pub pending: Vec<MigrationRow>,
    pub highest_applied: Option<i64>,
}

impl MigrationStatus {
    /// True when at least one migration is pending.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// True when no migration is applied (fresh DB).
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}
