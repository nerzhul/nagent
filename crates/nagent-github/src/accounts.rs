//! Multi-account model for the GitHub integration (plan
//! 1795793328177): several named PATs stored in one vault field
//! (`github.accounts`, a JSON array), a default-account pointer
//! (`github.default_account`), and a repo → account resolution map
//! (`github.repo_accounts`).
//!
//! This module holds the **pure** parts shared by the chat agents
//! (`nagent-agents`), the setup probes and the pure-API dashboard
//! (`nagent-server`):
//!
//! - [`AccountDef`] — one named token entry of the `accounts` JSON;
//! - [`parse_accounts`] / [`serialize_accounts`] — the single
//!   parse/serialize choke point for the vault blob;
//! - [`valid_account_name`] — the name charset
//!   `[a-z0-9][a-z0-9-_]{0,31}`;
//! - [`merge_tagged`] — the `account: "all"` fan-out merge: tag every
//!   item with its account, deduplicate by upstream `id`, sort by a
//!   timestamp field desc;
//! - [`merge_owners`] — the `?account=all` owners merge (dedupe by
//!   login).
//!
//! ## Credential redaction (AGENTS.md §6)
//!
//! The `accounts` blob contains several tokens. Parse and
//! validation errors therefore report the **key / index at
//! fault**, never the raw blob or a token value inside it.
//! `serialize_accounts` is the only function that ever reads the
//! token plaintext, and its output goes back into the encrypted
//! vault row — it is never logged, echoed, or formatted into an
//! error.

use secrecy::{ExposeSecret, SecretString};
use serde_json::{json, Value};

use crate::GithubError;

/// Name of the implicit account backed by
/// `github.personal_access_token`. Always present; can never be
/// deleted through the accounts endpoints.
pub const DEFAULT_ACCOUNT_NAME: &str = "default";

/// Special `account` argument value meaning "fan out across every
/// configured account and merge the results".
pub const ALL_ACCOUNTS: &str = "all";

/// One named account entry of the `github.accounts` vault blob.
///
/// The `token` is a `SecretString` so it is zeroised on drop and
/// never `Debug`-printed.
#[derive(Debug, Clone)]
pub struct AccountDef {
    pub name: String,
    pub token: SecretString,
    /// GitHub `login` captured by the probe when the account was
    /// added (display only).
    pub username: Option<String>,
    /// `X-OAuth-Scopes` header captured by the probe (display only).
    pub scopes: Option<String>,
}

/// Validate an account name: `[a-z0-9][a-z0-9-_]{0,31}`. The
/// `"default"` name is reserved (it maps to
/// `personal_access_token`) and rejected here so it can never be
/// stored inside `accounts`.
pub fn valid_account_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    name.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name != DEFAULT_ACCOUNT_NAME
}

/// Parse the `github.accounts` vault blob into validated entries.
///
/// `max_accounts` caps the array length. Errors carry the key /
/// index at fault, never the raw blob (AGENTS.md §6 — the blob
/// contains tokens): `GithubError::InvalidArguments` with a message
/// like `accounts[2].name is invalid`.
pub fn parse_accounts(blob: &str, max_accounts: usize) -> Result<Vec<AccountDef>, GithubError> {
    let trimmed = blob.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let parsed: Value = serde_json::from_str(trimmed).map_err(|_| {
        // Never include the raw blob: it contains tokens.
        GithubError::InvalidArguments(
            "github.accounts is not valid JSON (expected an array of {name, token, …})".to_string(),
        )
    })?;
    let arr = parsed.as_array().ok_or_else(|| {
        GithubError::InvalidArguments(
            "github.accounts must be a JSON array of {name, token, …} objects".to_string(),
        )
    })?;
    if arr.len() > max_accounts {
        return Err(GithubError::InvalidArguments(format!(
            "github.accounts holds {} entries but max_accounts is {max_accounts}",
            arr.len()
        )));
    }
    let mut out: Vec<AccountDef> = Vec::with_capacity(arr.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, entry) in arr.iter().enumerate() {
        let obj = entry.as_object().ok_or_else(|| {
            GithubError::InvalidArguments(format!("accounts[{i}] must be an object"))
        })?;
        let name = obj.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
            GithubError::InvalidArguments(format!("accounts[{i}].name is required"))
        })?;
        if !valid_account_name(name) {
            return Err(GithubError::InvalidArguments(format!(
                "accounts[{i}].name is invalid (charset [a-z0-9][a-z0-9-_]{{0,31}}, \
                 `\"{DEFAULT_ACCOUNT_NAME}\"` is reserved)"
            )));
        }
        if !seen.insert(name.to_string()) {
            return Err(GithubError::InvalidArguments(format!(
                "accounts[{i}].name `{name}` is duplicated"
            )));
        }
        let token = obj.get("token").and_then(|v| v.as_str()).ok_or_else(|| {
            GithubError::InvalidArguments(format!("accounts[{i}].token is required"))
        })?;
        if token.trim().is_empty() {
            return Err(GithubError::InvalidArguments(format!(
                "accounts[{i}].token must not be empty"
            )));
        }
        let username = obj
            .get("username")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let scopes = obj
            .get("scopes")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        out.push(AccountDef {
            name: name.to_string(),
            token: SecretString::from(token.to_string()),
            username,
            scopes,
        });
    }
    Ok(out)
}

/// Serialize account entries back into the vault blob shape. The
/// token plaintext only ever lands in the returned string, which
/// the caller writes into the AES-256-GCM-encrypted vault row —
/// never into a log, an error, or an HTTP response (AGENTS.md §6).
pub fn serialize_accounts(accounts: &[AccountDef]) -> String {
    let arr: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let mut obj = json!({
                "name": a.name,
                "token": a.token.expose_secret(),
            });
            if let Some(u) = &a.username {
                obj["username"] = Value::String(u.clone());
            }
            if let Some(s) = &a.scopes {
                obj["scopes"] = Value::String(s.clone());
            }
            obj
        })
        .collect();
    serde_json::to_string(&arr).unwrap_or_else(|_| "[]".to_string())
}

/// Public (token-free) projection of one account for
/// `GET …/accounts` and `github_list_accounts`: name, username,
/// scopes, is_default. Never carries the token (AGENTS.md §6).
pub fn account_meta(a: &AccountDef, is_default: bool) -> Value {
    json!({
        "name": a.name,
        "username": a.username,
        "scopes": a.scopes,
        "is_default": is_default,
    })
}

/// Tag every item of `items` with `"account": account`, deduplicate
/// by the upstream `id` field (first occurrence wins, in the order
/// the per-account lists arrive — i.e. default account first), and
/// sort by `sort_key` (an ISO 8601 timestamp) descending. Missing
/// keys sink to the bottom. This is the `account: "all"` merge for
/// `list_repos` / `list_notifications` / `search_issues`.
pub fn merge_tagged(per_account: Vec<(String, Vec<Value>)>, sort_key: &str) -> Vec<Value> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut merged: Vec<Value> = Vec::new();
    for (account, items) in per_account {
        for mut item in items {
            let id = item
                .get("id")
                .map(|v| v.to_string())
                .unwrap_or_else(|| item.to_string());
            if !seen.insert(id) {
                continue;
            }
            if let Some(obj) = item.as_object_mut() {
                obj.insert("account".to_string(), Value::String(account.clone()));
            }
            merged.push(item);
        }
    }
    merged.sort_by(|a, b| {
        let ka = a.get(sort_key).and_then(|v| v.as_str()).unwrap_or("");
        let kb = b.get(sort_key).and_then(|v| v.as_str()).unwrap_or("");
        kb.cmp(ka)
    });
    merged
}

/// Merge several `list_owners` payloads (`{"user": …, "owners":
/// […]}`) into one, deduplicating owners by `login` (first
/// occurrence wins) and keeping each owner's `via`. Used by
/// `GET …/owners?account=all`.
pub fn merge_owners(payloads: Vec<Value>) -> Value {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut owners: Vec<Value> = Vec::new();
    let mut user = String::new();
    for p in payloads {
        if user.is_empty() {
            user = p
                .get("user")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
        }
        for o in p
            .get("owners")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            if let Some(login) = o.get("login").and_then(|v| v.as_str()) {
                if seen.insert(login.to_string()) {
                    owners.push(o);
                }
            }
        }
    }
    json!({ "user": user, "owners": owners })
}

/// Merge several per-owner repo lists (`{"owner": …, "repos": […]}`)
/// into one, deduplicating repos by `full_name`. Used by
/// `GET …/owners/{owner}/repos?account=all`.
pub fn merge_owner_repos(payloads: Vec<Value>, owner: &str) -> Value {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut repos: Vec<Value> = Vec::new();
    for p in payloads {
        for r in p
            .get("repos")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            if let Some(name) = r.get("full_name").and_then(|v| v.as_str()) {
                if seen.insert(name.to_string()) {
                    repos.push(r);
                }
            }
        }
    }
    json!({ "owner": owner, "count": repos.len(), "repos": repos })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOK_A: &str = "ghp_account_a_token_aaaaaaaaaaaa";
    const TOK_B: &str = "ghp_account_b_token_bbbbbbbbbbbb";

    #[test]
    fn valid_account_name_accepts_charset_and_rejects_reserved() {
        assert!(valid_account_name("a"));
        assert!(valid_account_name("client-a"));
        assert!(valid_account_name("client_a2"));
        assert!(valid_account_name("0"));
        assert!(!valid_account_name("default"), "`default` is reserved");
        assert!(!valid_account_name("Client-A"), "uppercase rejected");
        assert!(!valid_account_name("-leading"));
        assert!(!valid_account_name("has space"));
        assert!(!valid_account_name(""));
        assert!(!valid_account_name(&"a".repeat(33)), "too long");
        assert!(valid_account_name(&"a".repeat(32)));
    }

    #[test]
    fn parse_accounts_roundtrip() {
        let blob = serialize_accounts(&[
            AccountDef {
                name: "client-a".into(),
                token: SecretString::from(TOK_A),
                username: Some("octo-a".into()),
                scopes: Some("repo, read:user".into()),
            },
            AccountDef {
                name: "client-b".into(),
                token: SecretString::from(TOK_B),
                username: None,
                scopes: None,
            },
        ]);
        let parsed = parse_accounts(&blob, 8).expect("parses");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].name, "client-a");
        assert_eq!(parsed[0].token.expose_secret(), TOK_A);
        assert_eq!(parsed[0].username.as_deref(), Some("octo-a"));
        assert_eq!(parsed[1].scopes, None);
    }

    #[test]
    fn parse_accounts_rejects_bad_json_without_leaking_the_blob() {
        let blob = format!("{{not json, token: \"{TOK_A}\"}}");
        let err = parse_accounts(&blob, 8).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains(TOK_A),
            "AGENTS.md §6: raw blob must not leak: {msg}"
        );
        assert!(msg.contains("not valid JSON"), "got {msg}");
    }

    #[test]
    fn parse_accounts_rejects_non_array_without_leaking() {
        let blob = format!("{{\"token\": \"{TOK_A}\"}}");
        let err = parse_accounts(&blob, 8).unwrap_err();
        assert!(!err.to_string().contains(TOK_A));
        assert!(err.to_string().contains("must be a JSON array"));
    }

    #[test]
    fn parse_accounts_enforces_cap() {
        let blob = serialize_accounts(&[AccountDef {
            name: "a".into(),
            token: SecretString::from(TOK_A),
            username: None,
            scopes: None,
        }]);
        let arr = format!("[{},{}]", blob, blob);
        // parse with max_accounts = 1 must fail; the blob itself
        // carries the token so the error must not echo it.
        let err = parse_accounts(&arr, 1).unwrap_err();
        assert!(err.to_string().contains("max_accounts"));
        assert!(!err.to_string().contains(TOK_A));
    }

    #[test]
    fn parse_accounts_rejects_bad_name_charset_dupes_and_empty_token() {
        // bad charset
        let err = parse_accounts(
            &format!("[{{\"name\": \"Bad Name\", \"token\": \"{TOK_A}\"}}]"),
            8,
        )
        .unwrap_err();
        assert!(err.to_string().contains("accounts[0].name"));
        assert!(!err.to_string().contains(TOK_A));
        // reserved name
        let err = parse_accounts(
            &format!("[{{\"name\": \"default\", \"token\": \"{TOK_A}\"}}]"),
            8,
        )
        .unwrap_err();
        assert!(err.to_string().contains("reserved"));
        // duplicate names
        let one = format!("[{{\"name\": \"a\", \"token\": \"{TOK_A}\"}}, {{\"name\": \"a\", \"token\": \"{TOK_B}\"}}]");
        let err = parse_accounts(&one, 8).unwrap_err();
        assert!(err.to_string().contains("duplicated"));
        assert!(!err.to_string().contains(TOK_A) && !err.to_string().contains(TOK_B));
        // empty token
        let err = parse_accounts("[{\"name\": \"a\", \"token\": \"  \"}]", 8).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn parse_accounts_empty_blob_yields_empty_list() {
        assert!(parse_accounts("", 8).unwrap().is_empty());
        assert!(parse_accounts("   ", 8).unwrap().is_empty());
    }

    #[test]
    fn merge_tagged_dedupes_by_id_tags_and_sorts() {
        let a = json!([
            {"id": 1, "full_name": "octo/shared", "pushed_at": "2026-01-01T00:00:00Z"},
            {"id": 2, "full_name": "octo/only-a", "pushed_at": "2026-03-01T00:00:00Z"},
        ]);
        let b = json!([
            // same upstream id via the second token → dropped
            {"id": 1, "full_name": "octo/shared", "pushed_at": "2026-02-01T00:00:00Z"},
            {"id": 3, "full_name": "clientb/repo", "pushed_at": "2026-02-15T00:00:00Z"},
        ]);
        let merged = merge_tagged(
            vec![
                ("default".to_string(), a.as_array().unwrap().clone()),
                ("client-b".to_string(), b.as_array().unwrap().clone()),
            ],
            "pushed_at",
        );
        assert_eq!(merged.len(), 3, "dedupe by id");
        assert_eq!(
            merged[0]["full_name"], "octo/only-a",
            "sorted desc by pushed_at"
        );
        assert_eq!(merged[0]["account"], "default");
        assert_eq!(merged[1]["full_name"], "clientb/repo");
        assert_eq!(merged[1]["account"], "client-b");
        // the shared id keeps the FIRST occurrence (default account)
        assert_eq!(merged[2]["full_name"], "octo/shared");
        assert_eq!(merged[2]["account"], "default");
        assert_eq!(merged[2]["pushed_at"], "2026-01-01T00:00:00Z");
    }

    #[test]
    fn merge_owners_dedupes_by_login() {
        let a = json!({"user": "octo", "owners": [
            {"login": "octo", "via": "self"}, {"login": "acme", "via": "org"}]});
        let b = json!({"user": "clientb", "owners": [
            {"login": "acme", "via": "org"}, {"login": "secretcorp", "via": "repo"}]});
        let merged = merge_owners(vec![a, b]);
        let logins: Vec<&str> = merged["owners"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["login"].as_str().unwrap())
            .collect();
        assert_eq!(logins, vec!["octo", "acme", "secretcorp"]);
        assert_eq!(merged["user"], "octo");
    }

    #[test]
    fn merge_owner_repos_dedupes_by_full_name() {
        let a = json!({"owner": "acme", "repos": [{"full_name": "acme/one"}, {"full_name": "acme/two"}]});
        let b = json!({"owner": "acme", "repos": [{"full_name": "acme/two"}, {"full_name": "acme/three"}]});
        let merged = merge_owner_repos(vec![a, b], "acme");
        assert_eq!(merged["count"], 3);
        let names: Vec<&str> = merged["repos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["full_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["acme/one", "acme/two", "acme/three"]);
    }

    #[test]
    fn account_meta_never_carries_the_token() {
        let a = AccountDef {
            name: "client-a".into(),
            token: SecretString::from(TOK_A),
            username: Some("octo".into()),
            scopes: Some("repo".into()),
        };
        let meta = account_meta(&a, true);
        let s = meta.to_string();
        assert!(
            !s.contains(TOK_A),
            "AGENTS.md §6: meta must not carry the token"
        );
        assert_eq!(meta["name"], "client-a");
        assert_eq!(meta["is_default"], true);
    }
}
