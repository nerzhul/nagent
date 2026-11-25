//! `github_read_file` — `GET /repos/{owner}/{repo}/contents/{path}`.
//!
//! The contents API returns the file body base64-encoded
//! (with embedded newlines); this tool decodes it and caps the
//! decoded text at `[agents.github].max_body_bytes`.

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};

use super::client::{truncate, GithubClient};
use super::{confirm_reason, parse_repo, run};
use crate::agents::{Agent, AgentError, UserContext};

pub struct ReadFileAgent {
    client: GithubClient,
}

impl ReadFileAgent {
    pub fn new(client: GithubClient) -> Self {
        Self { client }
    }
}

/// Decode the base64 `content` field of a contents API response
/// (GitHub inserts `\n` every 60 chars — stripped before
/// decoding). Returns the decoded bytes as a lossy UTF-8 string.
fn decode_content(content: &str) -> Result<String, AgentError> {
    let cleaned: String = content.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| {
            AgentError::AgentFailed(format!("github read_file: invalid base64 content: {e}"))
        })?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[async_trait]
impl Agent for ReadFileAgent {
    fn name(&self) -> &str {
        "github_read_file"
    }

    fn description(&self) -> &str {
        "Read a file's content from a GitHub repository \
         (`GET /repos/{owner}/{repo}/contents/{path}`). The base64 body is decoded \
         server-side and capped at [agents.github].max_body_bytes. Args: `repo` \
         (`\"owner/repo\"`), `path` (repo-relative, e.g. `src/main.rs` or `README.md`), \
         `ref?` (branch / tag / commit SHA, default the repo's default branch). \
         Returns path, sha, size, encoding, truncation flag, and the text content. \
         Read-only."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "Repository as `\"owner/repo\"`.",
                },
                "path": {
                    "type": "string",
                    "description": "Repo-relative file path, e.g. `README.md` or `src/main.rs`.",
                },
                "ref": {
                    "type": "string",
                    "description": "Optional branch, tag, or commit SHA to read from. Defaults to the repo's default branch.",
                },
            },
            "required": ["repo", "path"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "github", "file", "read", "code", "content", "source", "blob", "path",
        ]
    }

    fn requires_confirmation(
        &self,
        _ctx: &UserContext,
        _args: &Value,
    ) -> crate::agents::ConfirmationDecision {
        confirm_reason(self.name())
    }

    fn untrusted_output(&self) -> bool {
        // File contents come from a remote repo the LLM does not
        // own — the untrusted-input fence wraps the result.
        true
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let (owner, repo) = parse_repo(&args)?;
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('/') && !s.contains(".."))
            .ok_or_else(|| {
                AgentError::InvalidArguments(
                    "`path` is required (repo-relative, no leading `/`, no `..`)".into(),
                )
            })?
            .to_string();
        let git_ref = args
            .get("ref")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let api_path = format!("/repos/{owner}/{repo}/contents/{path}");
        let mut query = Vec::new();
        if let Some(r) = &git_ref {
            query.push(("ref".to_string(), r.clone()));
        }
        run(&self.client, ctx, self.name(), &args, &api_path, query, move |raw, cfg| {
            let content_type = raw.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if content_type != "file" {
                return json!({
                    "ok": false,
                    "error": format!("`{path}` is a `{content_type}`, not a file; use github_get_repo or github_list_repos for directories"),
                });
            }
            let encoded = raw.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let encoding = raw.get("encoding").and_then(|v| v.as_str()).unwrap_or("");
            match decode_content(encoded) {
                Ok(text) if encoding == "base64" => {
                    let truncated = text.len() > cfg.max_body_bytes;
                    json!({
                        "ok": true,
                        "data": {
                            "repo": format!("{owner}/{repo}"),
                            "path": raw.get("path"),
                            "name": raw.get("name"),
                            "sha": raw.get("sha"),
                            "size_bytes": raw.get("size"),
                            "encoding": encoding,
                            "truncated": truncated,
                            "max_body_bytes": cfg.max_body_bytes,
                            "content": truncate(&text, cfg.max_body_bytes),
                        }
                    })
                }
                Ok(_) => json!({
                    "ok": false,
                    "error": format!("unsupported content encoding `{encoding}` for `{path}`"),
                }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_content_strips_embedded_newlines() {
        // GitHub wraps base64 at 60 chars with `\n`.
        let raw = base64::engine::general_purpose::STANDARD.encode("hello world, this is a test");
        let wrapped: String = raw
            .as_bytes()
            .chunks(8)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            decode_content(&wrapped).unwrap(),
            "hello world, this is a test"
        );
    }

    #[test]
    fn decode_content_rejects_invalid_base64() {
        assert!(decode_content("!!!not-base64!!!").is_err());
    }
}
