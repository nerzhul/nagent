//! Refresh-token client trait and shared types.
//!
//! The OIDC backend and the future X-timeline agent both need to
//! mint a fresh access token from a stored refresh token. The
//! wire-level details (endpoint URL, body fields, signature
//! requirements) differ per provider, so the [`RefreshTokenClient`]
//! trait lets each backend implement its own refresh call while
//! keeping the high-level "use this credential" path identical.
//!
//! PR1 ships the trait, the [`TokenSet`] / [`RefreshTokenError`]
//! types, and unit tests that exercise the dispatch logic without
//! performing any real HTTP round-trips. The OIDC impl will land
//! in PR2 alongside the rest of the OIDC backend; the X impl will
//! land with the agent itself.

use std::time::Duration;

/// A `Bearer`-style token bundle returned by the IdP / provider.
#[derive(Debug, Clone)]
pub struct TokenSet {
    /// The bearer access token. Sent verbatim in the
    /// `Authorization: Bearer …` header.
    pub access_token: String,
    /// The refresh token. `None` when the provider does not issue
    /// refresh tokens (some do not, and the client must re-authorise
    /// instead).
    pub refresh_token: Option<String>,
    /// Token lifetime. PR1 trusts the provider's value; PR2 will
    /// subtract a small safety margin so a long-running agent does
    /// not race the expiry.
    pub expires_in: Duration,
    /// Space-separated scope string, as returned in the token
    /// response. Optional — some providers omit the field.
    pub scope: Option<String>,
}

impl TokenSet {
    /// True when the access token's lifetime has elapsed. PR1
    /// callers should re-authorise (or call
    /// [`RefreshTokenClient::refresh`]) rather than rely on this
    /// signal alone; PR2 will add a safety margin.
    pub fn is_expired(&self, now: Duration) -> bool {
        now >= self.expires_in
    }
}

/// Errors surfaced by [`RefreshTokenClient::refresh`]. Concrete
/// implementations map provider-specific failure modes into these
/// variants so the caller can branch on a stable enum.
#[derive(Debug, thiserror::Error)]
pub enum RefreshTokenError {
    /// The provider rejected the refresh token. The caller should
    /// drop the stored token and prompt the user to re-authorise.
    #[error("refresh token rejected by provider: {0}")]
    Rejected(String),
    /// Network / transport / TLS failure.
    #[error("transport error during refresh: {0}")]
    Transport(String),
    /// The response body could not be parsed. Treated as a
    /// transient failure — the caller may retry.
    #[error("malformed refresh response: {0}")]
    Malformed(String),
}

/// Refresh-token client. Each backend (OIDC, X, …) implements
/// this trait to expose its provider-specific refresh dance.
///
/// The trait is async so the impl can perform the HTTP round-trip
/// on the runtime. Implementations are expected to be cheap to
/// clone (hold an `Arc<reqwest::Client>` + the static config) so
/// they live directly in `AuthState` / the agent's runtime state.
#[async_trait::async_trait]
pub trait RefreshTokenClient: Send + Sync {
    /// Exchange `refresh_token` for a fresh [`TokenSet`]. Returns
    /// [`RefreshTokenError::Rejected`] when the provider rejects
    /// the token (the caller should then drop it and prompt the
    /// user to re-authorise).
    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet, RefreshTokenError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only client that returns a pre-canned [`TokenSet`].
    /// Used to exercise the trait without performing any HTTP
    /// round-trips. The name is intentionally verbose — concrete
    /// impls should use a descriptive per-provider name.
    struct StaticClient {
        response: Result<TokenSet, RefreshTokenError>,
    }

    #[async_trait::async_trait]
    impl RefreshTokenClient for StaticClient {
        async fn refresh(&self, _refresh_token: &str) -> Result<TokenSet, RefreshTokenError> {
            match &self.response {
                Ok(t) => Ok(t.clone()),
                Err(e) => Err(match e {
                    RefreshTokenError::Rejected(msg) => RefreshTokenError::Rejected(msg.clone()),
                    RefreshTokenError::Transport(msg) => RefreshTokenError::Transport(msg.clone()),
                    RefreshTokenError::Malformed(msg) => RefreshTokenError::Malformed(msg.clone()),
                }),
            }
        }
    }

    #[test]
    fn token_set_is_expired_after_lifetime() {
        let ts = TokenSet {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires_in: Duration::from_secs(60),
            scope: None,
        };
        assert!(!ts.is_expired(Duration::from_secs(30)));
        assert!(ts.is_expired(Duration::from_secs(60)));
        assert!(ts.is_expired(Duration::from_secs(120)));
    }

    #[tokio::test]
    async fn static_client_returns_ok() {
        let client = StaticClient {
            response: Ok(TokenSet {
                access_token: "new".into(),
                refresh_token: Some("r2".into()),
                expires_in: Duration::from_secs(3600),
                scope: Some("read".into()),
            }),
        };
        let ts = client.refresh("ignored").await.expect("ok");
        assert_eq!(ts.access_token, "new");
        assert_eq!(ts.refresh_token.as_deref(), Some("r2"));
        assert_eq!(ts.scope.as_deref(), Some("read"));
    }

    #[tokio::test]
    async fn static_client_returns_rejected() {
        let client = StaticClient {
            response: Err(RefreshTokenError::Rejected("invalid_grant".into())),
        };
        match client.refresh("ignored").await {
            Err(RefreshTokenError::Rejected(msg)) => assert_eq!(msg, "invalid_grant"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }
}
