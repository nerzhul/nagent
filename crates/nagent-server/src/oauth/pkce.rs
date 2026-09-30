//! PKCE (RFC 7636) verifier + S256 challenge generator.
//!
//! Used by the OIDC backend and (in the future) the X-timeline
//! agent to bind an authorization request to the same client that
//! redeems the code, so a stolen `code` alone is useless. PR1 only
//! needs the primitives — the wire calls in
//! [`crate::auth::oidc`] land in PR2.

use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// Length of the PKCE verifier. RFC 7636 §4.1 mandates 43..=128
/// unreserved characters; 64 bytes encoded as base64-url gives
/// exactly 86 characters — comfortably inside the bounds and large
/// enough to make brute-force infeasible.
pub const VERIFIER_LEN: usize = 64;

/// Base64-URL engine (no padding). RFC 7636 §4.2 / RFC 4648 §5.
fn b64url() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
}

/// Pair of PKCE primitives returned by [`PkcePair::generate`]. Both
/// halves are 86-character base64-URL strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    /// Plaintext verifier. Sent to the IdP on the token request
    /// (`code_verifier`).
    pub verifier: PkceVerifier,
    /// SHA-256 verifier, base64-URL-encoded. Sent to the IdP on the
    /// authorization request (`code_challenge`) with method
    /// `S256`.
    pub challenge: String,
}

/// Newtype around the verifier string. Prevents accidental
/// confusion with the challenge (a single character differs but the
/// blast radius is very different).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkceVerifier(String);

impl PkceVerifier {
    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl PkcePair {
    /// Generate a fresh verifier + S256 challenge. Each call
    /// returns a unique pair backed by OS-RNG bytes
    /// (`rand::rngs::OsRng`).
    pub fn generate() -> Self {
        let mut bytes = [0u8; VERIFIER_LEN];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let verifier = PkceVerifier(b64url().encode(bytes));
        let challenge = challenge_from_verifier(&verifier);
        Self {
            verifier,
            challenge,
        }
    }

    /// Compute the S256 challenge for a given verifier. Exposed so
    /// tests can check the encoding round-trip without spinning up
    /// OS-RNG.
    pub fn challenge_from_verifier(verifier: &PkceVerifier) -> String {
        challenge_from_verifier(verifier)
    }

    /// Verify that a challenge matches the verifier (the round-trip
    /// the IdP performs on the token request). Returns `true` on
    /// match. Constant-time comparison is not required here — the
    /// challenge is not a secret.
    pub fn verify(&self, challenge: &str) -> bool {
        // The IdP sends back the *challenge* it received on the
        // /authorize call; we recompute the challenge from our
        // stored verifier and compare. If they match, the IdP's
        // record is consistent with ours.
        challenge == self.challenge
    }
}

fn challenge_from_verifier(verifier: &PkceVerifier) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.0.as_bytes());
    let digest = hasher.finalize();
    b64url().encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_pair_is_well_formed() {
        let pair = PkcePair::generate();
        // Verifier must be 86 characters (64 bytes → 86 base64-url
        // chars, no padding).
        assert_eq!(pair.verifier.as_str().len(), 86);
        // Challenge: SHA-256 → 32 bytes → 43 base64-url chars.
        assert_eq!(pair.challenge.len(), 43);
        // Base64-url alphabet: A-Z, a-z, 0-9, -, _.
        for c in pair.verifier.as_str().chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "verifier contains non-base64-url char {c:?}"
            );
        }
    }

    #[test]
    fn two_generated_pairs_differ() {
        // OS-RNG should produce independent bytes for two adjacent
        // calls. Collisions would be a critical bug, not a flaky test.
        let a = PkcePair::generate();
        let b = PkcePair::generate();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn challenge_matches_verifier() {
        let pair = PkcePair::generate();
        // Recomputing from the same verifier yields the same
        // challenge.
        let recomputed = PkcePair::challenge_from_verifier(&pair.verifier);
        assert_eq!(recomputed, pair.challenge);
        // verify() must accept it.
        assert!(pair.verify(&pair.challenge));
    }

    #[test]
    fn verify_rejects_different_challenge() {
        let pair = PkcePair::generate();
        let other = PkcePair::generate();
        assert!(!pair.verify(&other.challenge));
    }

    #[test]
    fn deterministic_fixtures() {
        // Hard-coded vectors from RFC 7636 §4.6 with S256:
        //
        //   verifier   = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        //   challenge  = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        //
        // We reproduce the example so a regression to the encoding
        // surfaces as a unit-test failure rather than as a
        // cryptic IdP rejection in production.
        let verifier = PkceVerifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        let pair = PkcePair {
            verifier: verifier.clone(),
            challenge: PkcePair::challenge_from_verifier(&verifier),
        };
        assert_eq!(
            pair.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
