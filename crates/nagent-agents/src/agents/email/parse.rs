//! RFC 5322 / IMAP `SEARCH` parsing helpers shared by the email
//! agents.
//!
//! The list_inbox / get_message / search / mark_read agents reuse
//! the same header-decoding primitives, so the helpers live here
//! once. The `mailparse` crate does the heavy lifting for the
//! RFC 5322 body decoding; this module adds the small bits the
//! LLM-facing agents need on top of `mailparse`'s
//! `ParsedMail`/`MailHeader` API.
//!
//! ## `validate_search_criteria`
//!
//! The IMAP `SEARCH` command takes a structured query that the
//! server executes against its message store. Free-form queries
//! (e.g. Gmail-style `from:alice subject:report`) are deliberately
//! **v2** — implementing them properly would mean re-importing
//! the iCloud / Gmail search grammar. v1 only accepts a small
//! allow-list of IMAP criteria tokens:
//!
//! - `ALL` — every message in the folder.
//! - `SUBJECT <text>` — substring match on the `Subject:` header.
//! - `FROM <text>` — substring match on the `From:` header.
//! - `TO <text>` — substring match on the `To:` header.
//! - `BODY <text>` — substring match on the message body.
//! - `SINCE <date>` — received on or after the date (RFC 3501
//!   date format `DD-Mon-YYYY`).
//! - `BEFORE <date>` — received before the date.
//! - `UID <set>` — UIDs to include.
//! - `UNSEEN` / `SEEN` — flag-based filters.
//!
//! The validator splits the user input on whitespace, lowercases
//! each token, and rejects anything outside the allow-list. The
//! free-text arguments are quoted (escaping internal quotes) so a
//! malicious query cannot smuggle additional criteria tokens
//! past the allow-list (e.g. `SUBJECT "evil OR FROM hacker"`).

use crate::agents::AgentError;

/// Validate an IMAP `SEARCH` criteria string against the v1
/// allow-list. Returns the criteria on success (with each free-
/// text argument re-quoted to be wire-safe) or an
/// `AgentError::InvalidArguments` with a hint naming the
/// supported tokens.
pub fn validate_search_criteria(criteria: &str) -> Result<String, AgentError> {
    let allowed_keywords = [
        "all", "subject", "from", "to", "body", "since", "before", "uid", "unseen", "seen",
    ];
    let mut out = String::with_capacity(criteria.len());
    let mut tokens = criteria.split_whitespace().peekable();
    while let Some(tok) = tokens.next() {
        let lower = tok.to_ascii_lowercase();
        // Strip a trailing `:` that the LLM may include
        // (`SUBJECT:foo`). The wire form does not use colons.
        let lower = lower.trim_end_matches(':');
        if !allowed_keywords.contains(&lower) {
            return Err(AgentError::InvalidArguments(format!(
                "IMAP SEARCH: token `{tok}` is not in the v1 allow-list. \
                 Supported tokens: {}. Free-form Gmail-style queries (`from:alice subject:report`) \
                 are deliberately v2 — see plan 1791566269874 §10 Q4.",
                allowed_keywords.join(", ")
            )));
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(lower);
        if matches!(
            lower,
            "subject" | "from" | "to" | "body" | "since" | "before" | "uid"
        ) {
            // The keyword takes one quoted argument.
            let arg = tokens.next().ok_or_else(|| {
                AgentError::InvalidArguments(format!("IMAP SEARCH: `{lower}` requires an argument"))
            })?;
            out.push(' ');
            out.push('"');
            // Escape any internal `"` so a crafted query cannot
            // smuggle additional criteria tokens past the
            // allow-list.
            for ch in arg.chars() {
                if ch == '"' || ch == '\\' {
                    out.push('\\');
                }
                out.push(ch);
            }
            out.push('"');
        }
    }
    if out.is_empty() {
        return Err(AgentError::InvalidArguments(
            "IMAP SEARCH: empty criteria string (use `ALL` to list every message)".into(),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_accepts_known_keywords() {
        for q in [
            "ALL",
            "UNSEEN",
            "SEEN",
            "subject report",
            "FROM alice@example.com",
            "body budget",
            "SINCE 01-Jan-2026",
            "BEFORE 31-Dec-2026",
            "UID 1:100",
        ] {
            assert!(validate_search_criteria(q).is_ok(), "query: {q}");
        }
    }

    #[test]
    fn allowlist_rejects_unknown_keywords() {
        for q in [
            "TEXT foo",
            "OR FROM alice FROM bob",
            "from:alice subject:report",
            "header X-Spam yes",
        ] {
            let err = validate_search_criteria(q).unwrap_err();
            assert!(matches!(err, AgentError::InvalidArguments(_)), "query: {q}");
        }
    }

    #[test]
    fn allowlist_rejects_empty_input() {
        let err = validate_search_criteria("   ").unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn allowlist_rejects_keyword_without_argument() {
        let err = validate_search_criteria("SUBJECT").unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn allowlist_quotes_free_text_argument() {
        // The output must quote the argument so a crafted
        // substring like `evil OR FROM hacker` cannot smuggle
        // additional criteria tokens past the allow-list. The
        // space inside the argument means the LLM has to
        // pass a single token (no whitespace); the validator
        // then wraps the argument in `"…"` to make the IMAP
        // server treat the whole substring as a single
        // search value rather than as multiple criteria.
        let out = validate_search_criteria("SUBJECT evil-OR-FROM-hacker").unwrap();
        assert_eq!(out, "subject \"evil-OR-FROM-hacker\"");
    }
}
