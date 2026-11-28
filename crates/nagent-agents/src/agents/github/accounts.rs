//! Multi-account resolution for the `github_*` tools (plan
//! 1795793328177).
//!
//! One choke point — [`resolve_account`] — maps the optional
//! `account` tool argument to a concrete `(name, token)` pair:
//!
//! 1. explicit `account` arg (a name in `github.accounts`, or
//!    `"default"` → `personal_access_token`);
//! 2. the `github.default_account` vault field;
//! 3. `"default"` (`personal_access_token`).
//!
//! Missing everywhere → `CredentialsMissing` naming the account so
//! the UI can prompt precisely. [`list_accounts`] returns the
//! token-free metadata (name / username / scopes / is_default) the
//! `github_list_accounts` tool and the accounts endpoints surface.
//!
//! ## Credential redaction (AGENTS.md §6)
//!
//! The `github.accounts` blob holds several tokens. It is parsed
//! with [`nagent_github::parse_accounts`], whose errors name the
//! key/index at fault and never the raw blob; the tokens only ever
//! travel inside `SecretString` into the `Authorization` header.

use secrecy::SecretString;
use serde_json::Value;

use nagent_github::{account_meta, parse_accounts, AccountDef, ALL_ACCOUNTS, DEFAULT_ACCOUNT_NAME};

use crate::agents::{AgentError, UserContext};
use crate::GithubAgentConfig;

/// One resolved account: its name plus the token to authenticate
/// with. The name is what the cache key, the confirmation card and
/// the `"account"` tags carry — never the token.
#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub token: SecretString,
}

/// Token-free metadata of one account (name / username / scopes /
/// is_default) — the only shape that may leave the process.
#[derive(Debug, Clone)]
pub struct AccountMeta {
    pub name: String,
    pub username: Option<String>,
    pub scopes: Option<String>,
    pub is_default: bool,
}

impl AccountMeta {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "username": self.username,
            "scopes": self.scopes,
            "is_default": self.is_default,
        })
    }
}

/// Read + parse the `github.accounts` vault blob. A missing field
/// yields an empty list; an unparsable blob yields an
/// `AgentError::InvalidArguments` that names the key at fault, never
/// the blob (AGENTS.md §6 — the blob contains tokens).
pub async fn vault_account_defs(
    ctx: &UserContext,
    cfg: &GithubAgentConfig,
) -> Result<Vec<AccountDef>, AgentError> {
    match ctx.secret("github", "accounts").await {
        Ok(Some(blob)) => {
            use secrecy::ExposeSecret;
            parse_accounts(blob.expose_secret(), cfg.max_accounts).map_err(AgentError::from)
        }
        // Missing field → no extra accounts (backward compatible:
        // only the default PAT exists).
        Ok(None) => Ok(Vec::new()),
        Err(AgentError::CredentialsMissing { .. }) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The `github.default_account` vault field, when set.
pub async fn vault_default_account(ctx: &UserContext) -> Option<String> {
    use secrecy::ExposeSecret;
    match ctx.secret("github", "default_account").await {
        Ok(Some(v)) => {
            let s = v.expose_secret().trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        }
        _ => None,
    }
}

/// Every configured account, default first: `personal_access_token`
/// as `"default"`, then the parsed `github.accounts` entries in
/// stored order. Fails with `CredentialsMissing` when even the
/// default PAT is absent.
pub async fn all_accounts(
    ctx: &UserContext,
    cfg: &GithubAgentConfig,
) -> Result<Vec<Account>, AgentError> {
    let mut out = Vec::new();
    if let Ok(Some(pat)) = ctx.secret("github", "personal_access_token").await {
        out.push(Account {
            name: DEFAULT_ACCOUNT_NAME.to_string(),
            token: pat,
        });
    }
    for def in vault_account_defs(ctx, cfg).await? {
        out.push(Account {
            name: def.name,
            token: def.token,
        });
    }
    if out.is_empty() {
        return Err(AgentError::CredentialsMissing {
            service: "github".to_string(),
            field: "personal_access_token".to_string(),
        });
    }
    Ok(out)
}

/// Resolve one account from the optional `account` argument.
/// Resolution order (plan §1.2): explicit arg → `default_account`
/// field → `"default"`. `account: "all"` is NOT handled here — the
/// fan-out tools call [`all_accounts`] directly.
pub async fn resolve_account(
    ctx: &UserContext,
    cfg: &GithubAgentConfig,
    account_arg: Option<&str>,
) -> Result<Account, AgentError> {
    // 1. explicit arg (`"all"` is handled by the caller via
    //    [`all_accounts`], never here)
    if let Some(name) = account_arg
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != ALL_ACCOUNTS)
    {
        return account_by_name(ctx, cfg, name).await;
    }
    // 2. default_account field
    if let Some(name) = vault_default_account(ctx).await {
        if let Ok(acc) = account_by_name(ctx, cfg, &name).await {
            return Ok(acc);
        }
        // fall through to "default" when the stored name no longer
        // resolves (e.g. the account was deleted without repointing)
    }
    // 3. "default"
    account_by_name(ctx, cfg, DEFAULT_ACCOUNT_NAME).await
}

/// Look up one account by name: `"default"` → the PAT field; any
/// other name must exist in `github.accounts`.
async fn account_by_name(
    ctx: &UserContext,
    cfg: &GithubAgentConfig,
    name: &str,
) -> Result<Account, AgentError> {
    if name == DEFAULT_ACCOUNT_NAME {
        let token = ctx
            .secret("github", "personal_access_token")
            .await?
            .ok_or_else(|| AgentError::CredentialsMissing {
                service: "github".to_string(),
                field: format!("personal_access_token (account `{DEFAULT_ACCOUNT_NAME}`)"),
            })?;
        return Ok(Account {
            name: DEFAULT_ACCOUNT_NAME.to_string(),
            token,
        });
    }
    for def in vault_account_defs(ctx, cfg).await? {
        if def.name == name {
            return Ok(Account {
                name: def.name,
                token: def.token,
            });
        }
    }
    // Build the list of known names for the error (names only,
    // never tokens — AGENTS.md §6).
    let known = all_accounts(ctx, cfg)
        .await
        .map(|v| {
            v.iter()
                .map(|a| a.name.clone())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_else(|_| DEFAULT_ACCOUNT_NAME.to_string());
    Err(AgentError::InvalidArguments(format!(
        "unknown GitHub account `{name}` (configured accounts: {known})"
    )))
}

/// Token-free metadata of every configured account, default first.
/// This is the shape `github_list_accounts` and
/// `GET /api/integrations/github/accounts` return — it never
/// contains a token (AGENTS.md §6).
pub async fn list_accounts(
    ctx: &UserContext,
    cfg: &GithubAgentConfig,
) -> Result<Vec<AccountMeta>, AgentError> {
    let default_name = vault_default_account(ctx)
        .await
        .unwrap_or_else(|| DEFAULT_ACCOUNT_NAME.to_string());
    let username_field = ctx
        .secret("github", "username")
        .await
        .ok()
        .flatten()
        .map(|s| {
            use secrecy::ExposeSecret;
            s.expose_secret().to_string()
        });
    let mut out: Vec<AccountMeta> = Vec::new();
    if ctx
        .secret("github", "personal_access_token")
        .await
        .ok()
        .flatten()
        .is_some()
    {
        out.push(AccountMeta {
            name: DEFAULT_ACCOUNT_NAME.to_string(),
            username: username_field,
            scopes: None,
            is_default: default_name == DEFAULT_ACCOUNT_NAME,
        });
    }
    for def in vault_account_defs(ctx, cfg).await? {
        let is_default = def.name == default_name;
        out.push(AccountMeta {
            name: def.name.clone(),
            username: def.username.clone(),
            scopes: def.scopes.clone(),
            is_default,
        });
        // keep account_meta referenced for the server-side shape parity
        let _ = account_meta(&def, is_default);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::SecretSource;
    use crate::ServiceRegistry;
    use std::sync::Arc;

    const PAT: &str = "ghp_default_token_0000000000000000";
    const TOK_A: &str = "ghp_account_a_token_aaaaaaaaaaaa";
    const TOK_B: &str = "ghp_account_b_token_bbbbbbbbbbbb";

    fn accounts_blob() -> String {
        format!(
            "[{{\"name\":\"client-a\",\"token\":\"{TOK_A}\",\"username\":\"octo-a\",\"scopes\":\"repo\"}},\
             {{\"name\":\"client-b\",\"token\":\"{TOK_B}\"}}]"
        )
    }

    struct StaticSecrets {
        accounts: Option<String>,
        default_account: Option<String>,
    }

    #[async_trait::async_trait]
    impl SecretSource for StaticSecrets {
        async fn fetch(
            &self,
            _user_id: uuid::Uuid,
            service: &str,
            field: &str,
        ) -> Result<Option<SecretString>, AgentError> {
            if service != "github" {
                return Ok(None);
            }
            let v = match field {
                "personal_access_token" => Some(PAT.to_string()),
                "username" => Some("octo".to_string()),
                "accounts" => self.accounts.clone(),
                "default_account" => self.default_account.clone(),
                _ => None,
            };
            Ok(v.map(SecretString::from))
        }
    }

    fn ctx_with(secrets: StaticSecrets) -> UserContext {
        UserContext::new(
            uuid::Uuid::new_v4(),
            Arc::new(ServiceRegistry::empty()),
            Arc::new(secrets),
        )
    }

    fn cfg() -> GithubAgentConfig {
        GithubAgentConfig::default()
    }

    #[tokio::test]
    async fn resolution_order_arg_then_default_account_then_default() {
        // arg wins
        let ctx = ctx_with(StaticSecrets {
            accounts: Some(accounts_blob()),
            default_account: Some("client-b".into()),
        });
        let acc = resolve_account(&ctx, &cfg(), Some("client-a"))
            .await
            .unwrap();
        assert_eq!(acc.name, "client-a");
        assert_eq!(acc.token.expose_secret(), TOK_A);
        // default_account field wins over "default"
        let acc = resolve_account(&ctx, &cfg(), None).await.unwrap();
        assert_eq!(acc.name, "client-b");
        // no field → "default"
        let ctx = ctx_with(StaticSecrets {
            accounts: Some(accounts_blob()),
            default_account: None,
        });
        let acc = resolve_account(&ctx, &cfg(), None).await.unwrap();
        assert_eq!(acc.name, "default");
        assert_eq!(acc.token.expose_secret(), PAT);
        // explicit "default" works even with a default_account set
        let ctx = ctx_with(StaticSecrets {
            accounts: Some(accounts_blob()),
            default_account: Some("client-b".into()),
        });
        let acc = resolve_account(&ctx, &cfg(), Some("default"))
            .await
            .unwrap();
        assert_eq!(acc.name, "default");
    }

    #[tokio::test]
    async fn unknown_account_error_lists_names_never_tokens() {
        let ctx = ctx_with(StaticSecrets {
            accounts: Some(accounts_blob()),
            default_account: None,
        });
        let err = resolve_account(&ctx, &cfg(), Some("nope"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("nope") && msg.contains("client-a"),
            "got {msg}"
        );
        assert!(
            !msg.contains(TOK_A) && !msg.contains(TOK_B) && !msg.contains(PAT),
            "AGENTS.md §6: error must not contain tokens; got {msg}"
        );
    }

    #[tokio::test]
    async fn missing_everything_is_credentials_missing() {
        struct NoSecrets;
        #[async_trait::async_trait]
        impl SecretSource for NoSecrets {
            async fn fetch(
                &self,
                _u: uuid::Uuid,
                _s: &str,
                _f: &str,
            ) -> Result<Option<SecretString>, AgentError> {
                Ok(None)
            }
        }
        let ctx = UserContext::new(
            uuid::Uuid::new_v4(),
            Arc::new(ServiceRegistry::empty()),
            Arc::new(NoSecrets),
        );
        let err = resolve_account(&ctx, &cfg(), None).await.unwrap_err();
        assert!(
            matches!(err, AgentError::CredentialsMissing { .. }),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn all_accounts_default_first_then_extras() {
        let ctx = ctx_with(StaticSecrets {
            accounts: Some(accounts_blob()),
            default_account: None,
        });
        let all = all_accounts(&ctx, &cfg()).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].name, "default");
        assert_eq!(all[1].name, "client-a");
        assert_eq!(all[2].name, "client-b");
    }

    #[tokio::test]
    async fn list_accounts_metadata_never_contains_tokens() {
        let ctx = ctx_with(StaticSecrets {
            accounts: Some(accounts_blob()),
            default_account: Some("client-a".into()),
        });
        let metas = list_accounts(&ctx, &cfg()).await.unwrap();
        assert_eq!(metas.len(), 3);
        assert!(!metas[0].is_default);
        assert_eq!(metas[0].name, "default");
        assert_eq!(metas[0].username.as_deref(), Some("octo"));
        assert!(metas[1].is_default, "client-a is the default_account");
        assert_eq!(metas[1].scopes.as_deref(), Some("repo"));
        let json =
            serde_json::to_string(&metas.iter().map(|m| m.to_json()).collect::<Vec<_>>()).unwrap();
        assert!(
            !json.contains(TOK_A) && !json.contains(TOK_B) && !json.contains(PAT),
            "AGENTS.md §6: metadata must not contain tokens; got {json}"
        );
    }

    #[tokio::test]
    async fn corrupt_accounts_blob_error_never_contains_blob() {
        let blob = format!("[{{\"name\":\"a\",\"token\":\"{TOK_A}\"}}, broken");
        let ctx = ctx_with(StaticSecrets {
            accounts: Some(blob.clone()),
            default_account: None,
        });
        let err = all_accounts(&ctx, &cfg()).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains(TOK_A),
            "AGENTS.md §6: blob must not leak; got {msg}"
        );
        assert!(!msg.contains("broken"), "raw blob must not leak; got {msg}");
    }

    use secrecy::ExposeSecret;
}
