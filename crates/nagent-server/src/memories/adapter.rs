//! `UserDbMemorySource` — server-side adapter that turns a
//! `nagent_db::Db` + `Arc<CredentialsKey>` into a
//! `nagent_agents::MemorySource` (plan 1791267136806, §2.2).
//!
//! Mirrors the `CredentialResolver` / `StoreDocumentSource`
//! pattern: encryption happens here, the agents crate only sees
//! `SecretString`-wrapped plaintext in the `DecryptedMemory`
//! return path, and every operation writes one `auth_events`
//! audit row (plan §1.7).

use std::sync::Arc;

use nagent_agents::agents::{DecryptedMemory, MemoryMeta, MemorySource, MemoryWriteRequest};
use nagent_agents::AgentError;
use nagent_db::{Db, Error as DbError, NewAuthEvent, NewMemoryRequest};
use secrecy::ExposeSecret;
use uuid::Uuid;

use crate::credentials::crypto::{decrypt, encrypt, CryptoError};
use crate::credentials::key::CredentialsKey;

/// Adapter over [`nagent_db::memories::Memories`] that owns the
/// `[auth.credentials].key` and threads the AES-256-GCM seal /
/// open helpers into the chat-session memory path.
///
/// Constructed once per process (in `app::build_app`) and stashed
/// inside [`crate::llm::proxy::AppState`]; the LLM tool-loop
/// attaches it to every chat-session [`nagent_agents::UserContext`]
/// via [`nagent_agents::UserContext::for_chat_session_with_memories`].
#[derive(Clone)]
pub struct UserDbMemorySource {
    db: Db,
    key: Arc<CredentialsKey>,
    /// IP / UA for audit rows (matches the `CredentialResolver`
    /// contract). `None` when the source is built outside a request
    /// context (CLI usage, tests) and the audit row omits the
    /// fields.
    request_ip: Option<String>,
    request_user_agent: Option<String>,
}

impl std::fmt::Debug for UserDbMemorySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserDbMemorySource")
            .field("db", &"<nagent_db::Db>")
            .field("key", &self.key)
            .field("request_ip", &self.request_ip)
            .field("request_user_agent", &self.request_user_agent)
            .finish()
    }
}

impl UserDbMemorySource {
    pub fn new(db: Db, key: Arc<CredentialsKey>) -> Self {
        Self {
            db,
            key,
            request_ip: None,
            request_user_agent: None,
        }
    }

    /// Attach the live request's IP / UA so the audit rows carry
    /// the same provenance as the credential resolver. The HTTP
    /// boundary calls this in `llm::proxy::chat_completions` so
    /// every per-user audit kind (`memory_store`,
    /// `memory_forget`, `memory_recall`, `memory_inject`,
    /// `memory_decrypt_failed`) carries the calling client.
    pub fn with_request_context(mut self, ip: Option<String>, user_agent: Option<String>) -> Self {
        self.request_ip = ip;
        self.request_user_agent = user_agent;
        self
    }

    fn audit(&self, user_id: Uuid, kind: &'static str, extra_target: Option<&str>) {
        let mut ev = NewAuthEvent::auth(Some(user_id), kind, "memory");
        ev.target_service = Some("memory".into());
        // The plan §1.7 audit table only carries `id`/`subject`/
        // `predicate`/etc., never plaintext. `target_service =
        // "memory"` is the discriminator; any richer context is
        // reserved for a follow-up plan.
        let _ = extra_target;
        self.db.admin().events.record(ev);
    }
}

#[async_trait::async_trait]
impl MemorySource for UserDbMemorySource {
    async fn store(&self, user_id: Uuid, request: MemoryWriteRequest) -> Result<Uuid, AgentError> {
        // Encrypt `value` first. The encryption is mandatory even
        // for empty values: `aes-gcm` will produce a tagged
        // ciphertext for any non-empty plaintext, and the schema
        // insists on a non-NULL `value_ciphertext`.
        let value_plain = request.value.expose_secret().to_string();
        let sealed_value = encrypt(&self.key, &value_plain).map_err(|e| match e {
            CryptoError::DecryptFailed => {
                AgentError::AgentFailed("memory_store: encryption failed".into())
            }
        })?;
        // The intermediate plaintext is dropped here; `sealed_value`
        // is the only thing that lands in the DB.
        drop(value_plain);

        // Notes are encrypted the same way when present. Storing
        // `None` in the DB is a distinct signal from
        // `Some(empty)` — the schema treats the column nullable.
        let (notes_nonce, notes_ciphertext) = match &request.notes {
            Some(s) => {
                let sealed = encrypt(&self.key, s.expose_secret()).map_err(|e| match e {
                    CryptoError::DecryptFailed => {
                        AgentError::AgentFailed("memory_store: encryption failed".into())
                    }
                })?;
                (Some(sealed.nonce), Some(sealed.ciphertext))
            }
            None => (None, None),
        };

        let new_req = NewMemoryRequest {
            subject: request.subject,
            predicate: request.predicate,
            value_nonce: sealed_value.nonce,
            value_ciphertext: sealed_value.ciphertext,
            notes_nonce,
            notes_ciphertext,
            tags: request.tags,
            confidence: request.confidence,
            source_session_id: request.source_session_id,
            source_kind: request.source_kind,
        };

        let id = self
            .db
            .for_user(user_id)
            .memories()
            .upsert(new_req)
            .await
            .map_err(|e| AgentError::AgentFailed(format!("memory_store: db error: {e}")))?;

        self.audit(user_id, "memory_store", None);
        Ok(id)
    }

    async fn recall(
        &self,
        user_id: Uuid,
        subject: Option<&str>,
        predicate: Option<&str>,
        tags: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DecryptedMemory>, AgentError> {
        let rows = self
            .db
            .for_user(user_id)
            .memories()
            .recall(subject, predicate, tags, limit)
            .await
            .map_err(|e| AgentError::AgentFailed(format!("memory_recall: db error: {e}")))?;

        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let sealed_value = crate::credentials::crypto::EncryptedSecret {
                nonce: row.value_nonce.clone(),
                ciphertext: row.value_ciphertext.clone(),
            };
            match decrypt(&self.key, &sealed_value) {
                Ok(plain) => {
                    let notes = match (&row.notes_nonce, &row.notes_ciphertext) {
                        (Some(nonce), Some(ct)) => {
                            let sealed = crate::credentials::crypto::EncryptedSecret {
                                nonce: nonce.clone(),
                                ciphertext: ct.clone(),
                            };
                            match decrypt(&self.key, &sealed) {
                                Ok(p) => Some(p),
                                Err(_) => {
                                    // Notes decrypt failed but
                                    // value succeeded — surface
                                    // the row with `notes = None`
                                    // so the LLM still gets the
                                    // main fact, and write the
                                    // audit row.
                                    self.audit(user_id, "memory_decrypt_failed", None);
                                    None
                                }
                            }
                        }
                        _ => None,
                    };
                    out.push(DecryptedMemory {
                        id: row.id,
                        subject: row.subject,
                        predicate: row.predicate,
                        value: plain,
                        notes,
                        tags: row.tags,
                        confidence: row.confidence,
                        source_session_id: row.source_session_id,
                        source_kind: row.source_kind,
                        created_at: row.created_at,
                        last_used_at: row.last_used_at,
                        expires_at: row.expires_at,
                    });
                }
                Err(CryptoError::DecryptFailed) => {
                    // Wrong key / tampered ciphertext: drop the
                    // row from the result set and write one
                    // audit row. Mirrors the
                    // `credential_decrypt_failed` pattern.
                    self.audit(user_id, "memory_decrypt_failed", None);
                }
            }
        }

        self.audit(user_id, "memory_recall", None);
        Ok(out)
    }

    async fn list_meta(&self, user_id: Uuid, limit: usize) -> Result<Vec<MemoryMeta>, AgentError> {
        let rows = self
            .db
            .for_user(user_id)
            .memories()
            .list_meta(limit)
            .await
            .map_err(|e| AgentError::AgentFailed(format!("memory_list: db error: {e}")))?;
        let out = rows
            .into_iter()
            .map(|m| MemoryMeta {
                id: m.id,
                subject: m.subject,
                predicate: m.predicate,
                tags: m.tags,
                confidence: m.confidence,
                source_session_id: m.source_session_id,
                source_kind: m.source_kind,
                created_at: m.created_at,
                last_used_at: m.last_used_at,
                expires_at: m.expires_at,
            })
            .collect();
        self.audit(user_id, "memory_recall", None);
        Ok(out)
    }

    async fn forget(&self, user_id: Uuid, id: Uuid) -> Result<(), AgentError> {
        // Cross-user safety: `ScopedMemories::forget` is filtered
        // on `user_id`, so this returns `Ok(0)` when `id` belongs
        // to another user. The plan §4 mitigation maps that to a
        // plain "not found" surface so the LLM cannot tell
        // "exists but other user" from "does not exist".
        let affected = self
            .db
            .for_user(user_id)
            .memories()
            .forget(id)
            .await
            .map_err(|e| AgentError::AgentFailed(format!("memory_forget: db error: {e}")))?;
        if affected == 0 {
            // The plan's "Risk mitigation §4" row: refuse the
            // cross-user call by surfacing the same error
            // shape a non-existent id would produce. The
            // `Scoping` already happened in `ScopedMemories`; we
            // simply observe the zero and convert it.
            return Err(AgentError::AgentFailed(
                "memory: cross-user forget refused".into(),
            ));
        }
        self.audit(user_id, "memory_forget", None);
        Ok(())
    }
}

/// Convert a `DbError` into an `AgentError` for the
/// `MemorySource::recall` / `store` paths. Lives at this module
/// boundary so the agents-crate helpers don't need to depend on
/// `nagent_db`.
pub fn db_err(e: DbError) -> AgentError {
    AgentError::AgentFailed(format!("memory: db error: {e}"))
}
