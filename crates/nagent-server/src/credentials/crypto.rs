//! AES-256-GCM seal / open helpers for the credentials vault.
//!
//! The wire format is `(nonce || ciphertext)`: the ciphertext already
//! carries the 16-byte authentication tag that `aes-gcm` appends, so
//! storing `nonce + ciphertext` is sufficient to round-trip any value
//! below the chunked-ciphertext limit (~64 GiB per nonce; we never
//! encrypt anything remotely that large).
//!
//! Failure modes:
//! - Wrong key → `CredentialError::DecryptFailed` (the `aes-gcm`
//! crate does not distinguish wrong-key from tampered-ciphertext —
//! both raise `aead::Error`).
//! - Tampered ciphertext → same `DecryptFailed` variant. The
//! audit row's `kind` is set to `"credential_decrypt_failed"` so
//! ops can spot the difference in aggregate.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::credentials::key::CredentialsKey;

/// Persisted ciphertext row. Both `nonce` (12 bytes) and `ciphertext`
/// (variable length, includes the trailing 16-byte GCM tag) are
/// stored as `BLOB` in the `user_credentials` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptedSecret {
    /// 12 random bytes generated per `seal` call. Re-used on `open`.
    pub nonce: Vec<u8>,
    /// AES-256-GCM output; the appended 16-byte tag is included.
    pub ciphertext: Vec<u8>,
}

/// Errors surfaced by [`decrypt`]. Only one variant — the underlying
/// `aes-gcm` crate collapses wrong-key, wrong-nonce, and
/// tampered-ciphertext into the same `aead::Error`.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// AES-GCM authentication failed. The caller maps this to
    /// `CredentialError::DecryptFailed` and writes an audit row of
    /// kind `"credential_decrypt_failed"`.
    #[error("AES-GCM decryption failed (wrong key, wrong nonce, or tampered ciphertext)")]
    DecryptFailed,
}

/// Encrypt a plaintext string with the supplied key.
///
/// Generates a fresh 12-byte random nonce on every call; callers
/// MUST NOT reuse nonces across encryptions of the same plaintext
/// (reused nonces catastrophically break GCM).
pub fn encrypt(key: &CredentialsKey, plaintext: &str) -> Result<EncryptedSecret, CryptoError> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_bytes()));
    let nonce_bytes: [u8; 12] = rand_bytes_12();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext.as_bytes(),
                aad: &[],
            },
        )
        .map_err(|_| CryptoError::DecryptFailed)?;
    Ok(EncryptedSecret {
        nonce: nonce_bytes.to_vec(),
        ciphertext,
    })
}

/// Decrypt an [`EncryptedSecret`] back to a plaintext [`SecretString`].
///
/// Returns `CryptoError::DecryptFailed` on authentication failure
/// (wrong key, wrong nonce, or tampered ciphertext). The error
/// intentionally does NOT carry the bad ciphertext so a caller
/// logging the error cannot accidentally persist it.
pub fn decrypt(
    key: &CredentialsKey,
    sealed: &EncryptedSecret,
) -> Result<SecretString, CryptoError> {
    if sealed.nonce.len() != 12 {
        return Err(CryptoError::DecryptFailed);
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key.as_bytes()));
    let nonce = Nonce::from_slice(&sealed.nonce);
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &sealed.ciphertext,
                aad: &[],
            },
        )
        .map_err(|_| CryptoError::DecryptFailed)?;
    // `SecretString` zeroises on drop. The intermediate `String` is
    // moved into `SecretString::new` and not retained elsewhere.
    let s = String::from_utf8(plaintext).map_err(|_| CryptoError::DecryptFailed)?;
    Ok(SecretString::new(s.into_boxed_str()))
}

/// 12 random bytes for a fresh nonce.
///
/// Uses the OS RNG directly (via `rand::rngs::OsRng`) so the call is
/// infallible and does not pull in a thread-local generator.
fn rand_bytes_12() -> [u8; 12] {
    use rand::RngCore;
    let mut buf = [0u8; 12];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    fn key(bytes: u8) -> crate::credentials::CredentialsKey {
        crate::credentials::CredentialsKey::from_bytes([bytes; 32])
    }

    #[test]
    fn seal_then_open_round_trips() {
        let k = key(0x42);
        let sealed = encrypt(&k, "hunter2").unwrap();
        let opened = decrypt(&k, &sealed).unwrap();
        assert_eq!(opened.expose_secret(), "hunter2");
    }

    #[test]
    fn different_plaintexts_produce_different_ciphertexts() {
        // AES-GCM with a fresh random nonce per call MUST produce
        // distinct ciphertexts for identical plaintexts (reused
        // nonces catastrophically break GCM).
        let k = key(0x01);
        let s1 = encrypt(&k, "same").unwrap();
        let s2 = encrypt(&k, "same").unwrap();
        assert_ne!(s1.nonce, s2.nonce);
        assert_ne!(s1.ciphertext, s2.ciphertext);
    }

    #[test]
    fn wrong_key_fails_decrypt() {
        let k1 = key(0x01);
        let k2 = key(0x02);
        let sealed = encrypt(&k1, "secret").unwrap();
        let err = decrypt(&k2, &sealed).unwrap_err();
        assert!(matches!(err, CryptoError::DecryptFailed));
    }

    #[test]
    fn tampered_ciphertext_fails_decrypt() {
        let k = key(0x33);
        let mut sealed = encrypt(&k, "secret").unwrap();
        // Flip a single bit in the ciphertext.
        if let Some(b) = sealed.ciphertext.get_mut(0) {
            *b ^= 0x01;
        }
        let err = decrypt(&k, &sealed).unwrap_err();
        assert!(matches!(err, CryptoError::DecryptFailed));
    }

    #[test]
    fn wrong_nonce_length_is_rejected() {
        let k = key(0x44);
        let sealed = encrypt(&k, "secret").unwrap();
        let mut bad = sealed.clone();
        bad.nonce = vec![0u8; 5];
        let err = decrypt(&k, &bad).unwrap_err();
        assert!(matches!(err, CryptoError::DecryptFailed));
    }
}
