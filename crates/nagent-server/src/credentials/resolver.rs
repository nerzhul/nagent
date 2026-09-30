//! Async reader that turns `(user_id, service_id, field_key)` into a
//! `SecretString`.
//!
//! Holds an [`AuthStore`] for the DB row + audit writes and a
//! [`CredentialsKey`] for AES-GCM. Every call writes one audit row
//! to `auth_events`:
//!
//! - successful decrypt → `kind = "credential_access"`, `target_service = service_id`
//! - no row in `user_credentials` → `kind = "credential_missing"`, `target_service = service_id`
//! - decrypt failure → `kind = "credential_decrypt_failed"`, `target_service = service_id`
//!
//! Audit rows are best-effort — the same `AuthStore::record_event`
//! fire-and-forget pattern the rest of the auth subsystem uses.

use std::sync::Arc;

use secrecy::SecretString;
use uuid::Uuid;

use crate::auth::error::AuthError;
use crate::auth::store::{AuthStore, NewAuthEvent};
use crate::credentials::cache::SecretCache;
use crate::credentials::crypto::{decrypt, CryptoError, EncryptedSecret};
use crate::credentials::key::CredentialsKey;

/// Public error type for `CredentialResolver` lookups.
///
/// The mapping to HTTP responses lives in the agent / route layers;
/// the resolver only cares about *why* the lookup failed.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// The (service, field) tuple exists in the registry but the
    /// user has not configured it. Surfaced to the LLM as
    /// `AgentError::CredentialsMissing { service }` so the chat UI
    /// can show a "configure <service>" link.
    #[error("credential not configured for service={service} field={field}")]
    Missing { service: String, field: String },
    /// The ciphertext row exists but AES-GCM authentication failed.
    /// Almost always means the server-side
    /// `[auth.credentials].key` was rotated without re-encrypting
    /// the rows; the row is now unrecoverable.
    #[error("decrypt failed for service={service} field={field}")]
    DecryptFailed { service: String, field: String },
    /// Underlying auth-store error.
    #[error("credential store error: {0}")]
    Store(#[from] AuthError),
}

/// Async resolver: per-(user, service, field) lookup. Cheap to
/// `Clone` because every field is `Arc`-backed; the resolver is
/// stashed inside [`crate::agents::UserContext`].
#[derive(Clone)]
pub struct CredentialResolver {
    store: AuthStore,
    key: Arc<CredentialsKey>,
    /// IP / UA for audit rows. The route handlers attach the live
    /// request's IP and UA; the tool-loop path passes whatever the
    /// HTTP layer attached. `None` means "no request context" (CLI
    /// usage, tests) and the audit row omits the fields.
    request_ip: Option<String>,
    request_user_agent: Option<String>,
}

impl std::fmt::Debug for CredentialResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialResolver")
            .field("store", &"<AuthStore>")
            .field("key", &self.key)
            .field("request_ip", &self.request_ip)
            .field("request_user_agent", &self.request_user_agent)
            .finish()
    }
}

impl CredentialResolver {
    /// Build a new resolver. `request_ip` / `request_user_agent`
    /// are optional and end up on the audit row only.
    pub fn new(
        store: AuthStore,
        key: Arc<CredentialsKey>,
        request_ip: Option<String>,
        request_user_agent: Option<String>,
    ) -> Self {
        Self {
            store,
            key,
            request_ip,
            request_user_agent,
        }
    }

    /// Look up a single plaintext field for a user.
    ///
    /// Returns:
    /// - `Ok(Some(plaintext))` on a successful decrypt;
    /// - `Ok(None)` when the field is absent (no audit row; this is
    ///   the steady-state "user has not configured this" case);
    /// - `Err(CredentialError::Missing)` when the calling agent
    ///   distinguishes "missing" from "absent" (currently the same
    ///   shape as `Ok(None)` but explicit);
    /// - `Err(CredentialError::DecryptFailed)` when AES-GCM rejected
    ///   the ciphertext (always audited).
    pub async fn get(
        &self,
        user_id: Uuid,
        service: &str,
        field: &str,
        cache: &SecretCache,
    ) -> Result<Option<SecretString>, CredentialError> {
        // Cache hit short-circuits everything — no DB read, no
        // decrypt, no audit row (the first lookup already wrote one).
        if let Some(hit) = cache.get(service, field) {
            return Ok(Some(hit));
        }
        let row = self
            .store
            .fetch_user_credential(user_id, service, field)
            .await?;
        let Some(sealed) = row else {
            // Missing: audit + return Ok(None) so the calling agent
            // can decide whether to fall through or surface a
            // user-facing error.
            self.audit(user_id, "credential_missing", service);
            return Err(CredentialError::Missing {
                service: service.to_string(),
                field: field.to_string(),
            });
        };
        let sealed = EncryptedSecret {
            nonce: sealed.nonce,
            ciphertext: sealed.ciphertext,
        };
        match decrypt(&self.key, &sealed) {
            Ok(plaintext) => {
                self.audit(user_id, "credential_access", service);
                cache.insert(service, field, plaintext.clone());
                Ok(Some(plaintext))
            }
            Err(CryptoError::DecryptFailed) => {
                self.audit(user_id, "credential_decrypt_failed", service);
                Err(CredentialError::DecryptFailed {
                    service: service.to_string(),
                    field: field.to_string(),
                })
            }
        }
    }

    fn audit(&self, user_id: Uuid, kind: &str, target_service: &str) {
        self.store.record_event(NewAuthEvent {
            user_id: Some(user_id),
            kind: kind.to_string(),
            provider: "credentials".to_string(),
            ip: self.request_ip.clone(),
            user_agent: self.request_user_agent.clone(),
            target_service: Some(target_service.to_string()),
        });
    }
}
