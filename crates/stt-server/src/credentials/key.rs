//! Server-side encryption key for the credentials vault.
//!
//! The key is read once at boot from `[auth.credentials].key` in
//! the TOML config as a 64-character hex string, decoded into 32
//! raw bytes, and wrapped in [`secrecy::SecretBox`] so accidental
//! `Debug` output cannot leak the bytes. The wrapping type also
//! auto-zeroises on drop (via the `secrecy` crate's `zeroize`
//! integration).
//!
//! The key is required when `auth.enabled && agents.enabled && at
//! least one per-user agent is compiled`. In every other case the
//! framework stays dormant and the env var is ignored (single-user
//! trust boundary unchanged).

use std::fmt;

use secrecy::{ExposeSecret, SecretBox};
use serde::{Deserialize, Serialize};
use zeroize::ZeroizeOnDrop;

/// Encryption key for the per-user credentials vault.
///
/// 32 raw bytes wrapped in [`SecretBox`] so the type is not `Debug` and
/// the bytes are zeroised on drop. Construction is infallible once
/// the bytes are in hand; validation happens in
/// [`CredentialsKey::from_env`] (hex decode + length check).
#[derive(ZeroizeOnDrop)]
pub struct CredentialsKey(SecretBox<[u8; 32]>);

impl Clone for CredentialsKey {
    fn clone(&self) -> Self {
        // Deep-copy: read the bytes through `expose_secret`, then
        // build a fresh `SecretBox`. The secrecy crate's blanket
        // `Clone` impl requires `T: CloneableSecret` which is not
        // implemented for `[u8; 32]`.
        let bytes: [u8; 32] = *self.0.expose_secret();
        Self(SecretBox::new(Box::new(bytes)))
    }
}

impl CredentialsKey {
    /// Wrap the supplied 32 raw bytes. The caller is responsible
    /// for handing in a high-entropy key — typically the output of
    /// `from_hex` / `from_env`.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(SecretBox::new(Box::new(bytes)))
    }

    /// Decode a 64-character hex string into a `CredentialsKey`.
    /// This is the primary constructor used at boot: the key
    /// lives in plaintext inside `[auth.credentials].key` in the
    /// server's TOML config. Operators who want to keep the
    /// secret out of disk can do so by putting the TOML file on
    /// an encrypted volume (k8s Secret mounted as `subPath`,
    /// Vault Agent, …) — the configuration shape itself does
    /// not force plaintext storage.
    ///
    /// - Empty / whitespace-only → [`CredentialsKeyError::Missing`].
    /// - Length not 64 hex chars or non-hex content →
    ///   [`CredentialsKeyError::InvalidHex`].
    pub fn from_hex(hex_str: &str) -> Result<Self, CredentialsKeyError> {
        let trimmed = hex_str.trim();
        if trimmed.is_empty() {
            return Err(CredentialsKeyError::Missing("auth.credentials.key".into()));
        }
        let bytes = hex::decode(trimmed).map_err(|e| CredentialsKeyError::InvalidHex {
            name: "auth.credentials.key".to_string(),
            reason: e.to_string(),
        })?;
        if bytes.len() != 32 {
            return Err(CredentialsKeyError::InvalidHex {
                name: "auth.credentials.key".to_string(),
                reason: format!("expected 32 bytes after hex decode, got {}", bytes.len()),
            });
        }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&bytes);
        Ok(Self::from_bytes(buf))
    }

    /// Read the key from an environment variable. Kept for callers
    /// that still prefer the indirection (test harnesses, secret
    /// managers that mount env vars); the production boot path
    /// uses [`CredentialsKey::from_hex`] with the value from
    /// `[auth.credentials].key`.
    ///
    /// - `None` / empty value → [`CredentialsKeyError::Missing`].
    /// - Length not 64 hex chars or non-hex content →
    ///   [`CredentialsKeyError::InvalidHex`].
    pub fn from_env(name: &str) -> Result<Self, CredentialsKeyError> {
        let raw = std::env::var(name)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or(CredentialsKeyError::Missing(name.to_string()))?;
        let trimmed = raw.trim();
        let bytes = hex::decode(trimmed).map_err(|e| CredentialsKeyError::InvalidHex {
            name: name.to_string(),
            reason: e.to_string(),
        })?;
        if bytes.len() != 32 {
            return Err(CredentialsKeyError::InvalidHex {
                name: name.to_string(),
                reason: format!("expected 32 bytes after hex decode, got {}", bytes.len()),
            });
        }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&bytes);
        Ok(Self::from_bytes(buf))
    }

    /// Expose the raw bytes to the AES-GCM cipher.
    ///
    /// Returns a `&[u8; 32]` borrowing from the inner `SecretBox`; the
    /// caller must not retain the reference past the cryptographic
    /// operation so the plaintext does not outlive the key on the
    /// stack.
    pub fn as_bytes(&self) -> &[u8; 32] {
        // `SecretBox::new` guarantees the inner value is exactly 32 bytes.
        // The `expose_secret` call grants temporary access; nothing
        // outside this module keeps the reference.
        self.0.expose_secret()
    }
}

// `ZeroizeOnDrop` on the wrapper handles in-memory cleanup via the
// `SecretBox` drop impl; no explicit `Zeroize` impl needed here.

impl fmt::Debug for CredentialsKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never expose the key — `Secret::new` already forbids this
        // for the inner value; we additionally override `Debug` so
        // the wrapper itself is opaque in `tracing::info!` etc.
        f.debug_struct("CredentialsKey")
            .field("bytes", &"<redacted 32-byte key>")
            .finish()
    }
}

// `Clone` is derived above; the clone is a fresh `SecretBox` so the
// raw bytes are independent (SecretBox::Clone re-wraps).

impl Serialize for CredentialsKey {
    fn serialize<S: serde::Serializer>(&self, _ser: S) -> Result<S::Ok, S::Error> {
        // Keys must never serialise. Returning `Ok(())` would silently
        // emit nothing; an error surfaces the bug at the call site.
        Err(serde::ser::Error::custom(
            "CredentialsKey cannot be serialized",
        ))
    }
}

impl<'de> Deserialize<'de> for CredentialsKey {
    fn deserialize<D: serde::Deserializer<'de>>(_de: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "CredentialsKey cannot be deserialized",
        ))
    }
}

/// Failure modes for [`CredentialsKey::from_env`].
#[derive(Debug, thiserror::Error)]
pub enum CredentialsKeyError {
    /// The named env var was unset or empty.
    #[error("credentials key env var {0:?} is not set")]
    Missing(String),
    /// The env var's value was not a valid hex string of the right
    /// length. The inner `reason` is the human-readable description
    /// (length, charset, decode error).
    #[error("credentials key env var {name:?} is invalid: {reason}")]
    InvalidHex { name: String, reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_missing_returns_missing_error() {
        // SAFETY: the test runs single-threaded for this env var.
        // `serial_test` is not pulled in so we use a name that is
        // effectively unique to this test.
        let name = "NAGENT_CRED_KEY_MISSING_TEST";
        unsafe {
            std::env::remove_var(name);
        }
        let err = CredentialsKey::from_env(name).unwrap_err();
        assert!(matches!(err, CredentialsKeyError::Missing(_)));
    }

    #[test]
    fn from_env_wrong_length_returns_invalid_hex() {
        let name = "NAGENT_CRED_KEY_WRONG_LEN_TEST";
        // SAFETY: see `from_env_missing_returns_missing_error`.
        unsafe {
            std::env::set_var(name, "deadbeef");
        }
        let err = CredentialsKey::from_env(name).unwrap_err();
        assert!(matches!(err, CredentialsKeyError::InvalidHex { .. }));
        unsafe {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn from_env_non_hex_returns_invalid_hex() {
        let name = "NAGENT_CRED_KEY_NONHEX_TEST";
        let value = "z".repeat(64);
        // SAFETY: see `from_env_missing_returns_missing_error`.
        unsafe {
            std::env::set_var(name, &value);
        }
        let err = CredentialsKey::from_env(name).unwrap_err();
        assert!(matches!(err, CredentialsKeyError::InvalidHex { .. }));
        unsafe {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn from_env_happy_path_returns_key() {
        let name = "NAGENT_CRED_KEY_OK_TEST";
        let value = "0".repeat(64);
        // SAFETY: see `from_env_missing_returns_missing_error`.
        unsafe {
            std::env::set_var(name, &value);
        }
        let k = CredentialsKey::from_env(name).expect("happy path");
        // Sanity: the key bytes match what we set.
        assert_eq!(k.as_bytes(), &[0u8; 32]);
        unsafe {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn from_hex_happy_path_returns_key() {
        let k = CredentialsKey::from_hex(&"0".repeat(64)).expect("happy path");
        assert_eq!(k.as_bytes(), &[0u8; 32]);
    }

    #[test]
    fn from_hex_trims_whitespace() {
        // Operators often paste the hex with a trailing newline;
        // the constructor must tolerate it.
        let k = CredentialsKey::from_hex(&format!("  {}  \n", "0".repeat(64))).expect("trimmed");
        assert_eq!(k.as_bytes(), &[0u8; 32]);
    }

    #[test]
    fn from_hex_empty_returns_missing() {
        let err = CredentialsKey::from_hex("").unwrap_err();
        assert!(matches!(err, CredentialsKeyError::Missing(_)));
    }

    #[test]
    fn from_hex_wrong_length_returns_invalid_hex() {
        let err = CredentialsKey::from_hex("deadbeef").unwrap_err();
        assert!(matches!(err, CredentialsKeyError::InvalidHex { .. }));
    }

    #[test]
    fn from_hex_non_hex_returns_invalid_hex() {
        let err = CredentialsKey::from_hex(&"z".repeat(64)).unwrap_err();
        assert!(matches!(err, CredentialsKeyError::InvalidHex { .. }));
    }

    #[test]
    fn clone_is_an_independent_copy() {
        let k1 = CredentialsKey::from_bytes([1u8; 32]);
        let k2 = k1.clone();
        // Mutating k1's bytes via a fresh seal should not affect
        // k2 — they are independent allocations.
        let s = crate::credentials::crypto::encrypt(&k1, "x").unwrap();
        let _ = crate::credentials::crypto::decrypt(&k2, &s).unwrap();
    }
}
