//! `wikipedia` agent: short summary of a Wikipedia article.
//!
//! Backed by Wikimedia's REST `page/summary` endpoint:
//!
//! ```text
//! GET https://en.wikipedia.org/api/rest_v1/page/summary/{title}
//! ```
//!
//! The endpoint returns a small JSON document with `title`, a plain
//! text `extract`, a one-line `description`, and `content_urls`
//! pointing at the full article. The agent trims the envelope to
//! those four fields — the LLM only needs the headline + the link —
//! and prefixes every record with the standard `ok`/`source`/
//! `fetched_at` shape the other agents use.
//!
//! ## Authentication and rate limits
//!
//! The endpoint is **anonymous** — no API key required. Wikimedia's
//! published policy is that unidentified clients are rate-limited
//! or 403'd, so the agent unconditionally sets a descriptive
//! `User-Agent` header identifying the application and a contact URL.
//! Operators can override the value via the `WIKIPEDIA_USER_AGENT`
//! env var / `wikipedia.user_agent` TOML key (defaults to
//! `nagent-wikipedia-agent/<version>`).
//!
//! ## Why not `action=query` (the legacy API)
//!
//! `action=query&prop=extracts` returns the same data but requires a
//! `format=json` parameter, returns the body wrapped in
//! `query.pages.{id}.extract[]`, and historically served a different
//! shape depending on the version. The REST endpoint is stable, is
//! documented at <https://en.wikipedia.org/api/rest_v1/>, and is the
//! one Wikimedia recommends for new clients.
//!
//! ## Sandbox
//!
//! The agent never accepts a user-controlled URL — the upstream
//! scheme + host are hardcoded in [`WikipediaConfig`]. The
//! `web_fetch` allow-list machinery therefore does not apply; this
//! is documented in the module header so a future security pass can
//! extend the allow-list machinery if the agent ever needs to.
//!
//! ## Latency
//!
//! One HTTP round-trip per query. The summary endpoint is on
//! Wikipedia's edge cache and typically returns in well under a
//! second.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError};
use crate::config::WikipediaConfig;

/// Hard cap on the response body. The summary endpoint returns at
/// most a few KiB; 64 KiB leaves room for occasional bloat while
/// still bounding memory.
const MAX_UPSTREAM_BYTES: usize = 64 * 1024;

/// Hard cap on the page title length. Wikipedia's own URL limit is
/// around 256 bytes; we cap further at 200 to leave room for the
/// `/page/summary/` path prefix.
const MAX_TITLE_LEN: usize = 200;

/// Build of the `wikipedia` agent.
#[derive(Clone)]
pub struct WikipediaAgent {
    cfg: WikipediaConfig,
    http: reqwest::Client,
}

impl std::fmt::Debug for WikipediaAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WikipediaAgent")
            .field("cfg", &self.cfg)
            .field("http", &"<reqwest::Client>")
            .finish()
    }
}

impl Default for WikipediaAgent {
    fn default() -> Self {
        Self::new(WikipediaConfig::default())
    }
}

impl WikipediaAgent {
    pub fn new(cfg: WikipediaConfig) -> Self {
        let timeout = Duration::from_millis(cfg.timeout_ms.max(1_000));
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .expect("reqwest client build");
        Self { cfg, http }
    }
}

#[async_trait]
impl Agent for WikipediaAgent {
    fn name(&self) -> &str {
        "wikipedia"
    }

    fn description(&self) -> &str {
        "Résumé court d'un article Wikipédia via l'API REST officielle \
         (https://en.wikipedia.org/api/rest_v1/page/summary/{title}). \
         Aucune clé d'API, seul un User-Agent descriptif est requis. \
         Renvoie le titre, l'extrait, une description en une ligne, et le lien \
         vers l'article complet. \
         Use for 'qui est Marie Curie ?', 'parle-moi de la Renaissance', 'what \
         is photosynthesis on Wikipedia', 'résumé de l'article Albert Einstein'. \
         Pass `title` (required, page title e.g. 'Marie Curie', 'Renaissance', \
         'Photosynthesis'). \
         NOT for cities-as-places (use `get_weather` for weather forecasts or \
         'où suis-je' for geolocation); this tool returns the encyclopedia \
         article ABOUT a subject, including people, events, concepts, works, \
         and historical places described as topics."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "maxLength": 200,
                    "pattern": "^[^\\x00]{1,200}$",
                    "description": "Wikipedia page title (case- and space-sensitive: 'Lyon' or 'Marie Curie'). Avoid the leading namespace prefix; 'Talk:Lyon' works but 'Lyon' is simpler."
                }
            },
            "required": ["title"],
            "additionalProperties": false,
        })
    }

    async fn invoke(&self, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let url = build_summary_url(&self.cfg.base_url, &req.title);
        let body = fetch_json(&self.http, &self.cfg.user_agent, &url).await?;
        let payload = shape_payload(&body, &req.title)?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "summary": build_summary(&payload, &req.title),
            "data": payload,
            "source": "wikipedia.org",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ---- URL builder ---------------------------------------------------------

/// Build the `/page/summary/{title}` URL against the configured base.
/// The title is percent-encoded so spaces become `%20` and accented
/// characters stay as UTF-8 bytes (which is what Wikimedia expects).
fn build_summary_url(base_url: &str, title: &str) -> String {
    let trimmed_base = base_url.trim_end_matches('/');
    let encoded = url_encode_path_segment(title);
    format!("{trimmed_base}/page/summary/{encoded}")
}

/// Minimal path-segment encoder. Unlike the weather/stock agents we
/// do need to encode `/` and `?` here because titles like "Marie
/// Curie" can carry spaces, slashes, or other reserved characters
/// (e.g. "AT&T" → "AT%26T", "C++" → "C%2B%2B").
fn url_encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        // RFC 3986 unreserved set + a few sub-delimiters Wikipedia
        // tolerates verbatim in the path. We percent-encode everything
        // else so the URL never carries an invalid byte.
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---- HTTP helper ---------------------------------------------------------

async fn fetch_json(
    http: &reqwest::Client,
    user_agent: &str,
    url: &str,
) -> Result<Value, AgentError> {
    let resp = http
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent)
        .header(reqwest::header::ACCEPT, "application/json; charset=utf-8; profile=\"https://www.mediawiki.org/wiki/Specs/Summary/1.5.0\"")
        .send()
        .await
        .map_err(|e| AgentError::AgentFailed(format!("upstream connect/read failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        // Wikimedia surfaces errors with a `type` and `title` field
        // (e.g. `{"type":"https://mediawiki.org/wiki/HyperSwitch/errors/not_found","title":"Not found."}`).
        // Surface the human-readable title so the LLM sees an
        // actionable message ("Not found." → suggest the user try a
        // different spelling).
        let parsed: Option<Value> = serde_json::from_str(&body).ok();
        let upstream_message = parsed
            .as_ref()
            .and_then(|v| v.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        return Err(AgentError::Upstream {
            status: status.as_u16(),
            body: if !upstream_message.is_empty() {
                upstream_message.to_string()
            } else {
                truncate(&body, 2048)
            },
        });
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| AgentError::AgentFailed(format!("read body: {e}")))?;
    if bytes.len() > MAX_UPSTREAM_BYTES {
        return Err(AgentError::AgentFailed(format!(
            "upstream body too large: {} bytes (cap {MAX_UPSTREAM_BYTES})",
            bytes.len()
        )));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| AgentError::AgentFailed(format!("upstream JSON parse: {e}")))
}

// ---- Payload shaping -----------------------------------------------------

/// Trim the upstream summary document to the four fields the LLM
/// actually needs. Anything missing is treated as `null` so the
/// envelope stays stable across schema variations.
fn shape_payload(body: &Value, requested_title: &str) -> Result<Value, AgentError> {
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or(requested_title)
        .to_string();
    let description = body
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let extract = body
        .get("extract")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            AgentError::AgentFailed(
                "Wikipedia summary response missing `extract` (title may be a disambiguation or list page)"
                    .into(),
            )
        })?
        .to_string();
    let page_url = body
        .get("content_urls")
        .and_then(|c| c.get("desktop"))
        .and_then(|d| d.get("page"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(json!({
        "title": title,
        "description": description,
        "extract": extract,
        "url": page_url,
    }))
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.len() <= max_chars {
        return s.to_string();
    }
    let mut end = max_chars;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Build a one-line summary suitable for the tool bubble. Prefers
/// the short `description` field ("city in France"); falls back to
/// the first sentence of `extract` when the description is empty.
fn build_summary(payload: &Value, requested_title: &str) -> String {
    let title = payload
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or(requested_title);
    let description = payload
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !description.is_empty() {
        return format!("{title} — {description}");
    }
    let extract = payload
        .get("extract")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // Take the first sentence (or first 80 chars) of the extract so
    // the bubble stays a single line.
    let first = extract
        .split_once(". ")
        .map(|(head, _)| head)
        .unwrap_or(extract);
    let first = first.trim();
    let first = if first.ends_with('.') {
        first.to_string()
    } else {
        format!("{first}.")
    };
    format!("{title} — {}", truncate(&first, 80))
}

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug)]
struct ParsedArgs {
    title: String,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let title = obj
        .get("title")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`title` (string) is required".into()))?
        .trim()
        .to_string();
    if title.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`title` must not be empty".into(),
        ));
    }
    if title.len() > MAX_TITLE_LEN {
        return Err(AgentError::InvalidArguments(format!(
            "`title` must be at most {MAX_TITLE_LEN} characters (got {})",
            title.len()
        )));
    }
    if title.contains('\0') {
        return Err(AgentError::InvalidArguments(
            "`title` contains a null byte".into(),
        ));
    }
    Ok(ParsedArgs { title })
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn name_and_schema_are_stable() {
        let agent = WikipediaAgent::default();
        assert_eq!(agent.name(), "wikipedia");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("title")));
        assert_eq!(schema["properties"]["title"]["maxLength"], 200);
    }

    #[test]
    fn description_does_not_route_cities_to_wikipedia() {
        // Regression guard: an earlier description listed "population
        // de Lyon" as a sample query, which made small open-source
        // LLMs route city questions to `wikipedia` AND weather
        // questions to `get_weather` interchangeably. The current
        // description must steer the LLM the other way: clear
        // biographical/encyclopedic examples, and an explicit "NOT
        // for cities" note so cities-as-places go to `get_weather`
        // / a geo tool.
        let agent = WikipediaAgent::default();
        let desc = agent.description();
        assert!(
            !desc.contains("Lyon"),
            "description must not list city examples; the LLM routes \
             cities to `get_weather` instead. got: {desc}"
        );
        assert!(
            desc.contains("NOT for cities"),
            "description must warn the LLM away from city queries; got: {desc}"
        );
        assert!(
            desc.contains("get_weather"),
            "description must point the LLM at `get_weather` for cities; got: {desc}"
        );
    }

    #[test]
    fn build_summary_url_encodes_spaces() {
        let url = build_summary_url("https://en.wikipedia.org/api/rest_v1", "Marie Curie");
        assert_eq!(
            url,
            "https://en.wikipedia.org/api/rest_v1/page/summary/Marie%20Curie"
        );
    }

    #[test]
    fn build_summary_url_handles_accents_and_specials() {
        // Accented characters must be preserved as UTF-8 (Wikimedia
        // expects this); reserved characters like `+` and `&` must
        // be percent-encoded.
        let url = build_summary_url("https://en.wikipedia.org/api/rest_v1", "C++");
        assert!(url.ends_with("C%2B%2B"));
        let url = build_summary_url("https://en.wikipedia.org/api/rest_v1", "AT&T");
        assert!(url.contains("AT%26T"));
        let url = build_summary_url("https://en.wikipedia.org/api/rest_v1", "Électricité");
        assert!(url.contains("%C3%89lectricit%C3%A9"));
    }

    #[test]
    fn build_summary_url_strips_trailing_slash_from_base() {
        // Operators often paste a base URL with a trailing slash;
        // the builder must not produce `//page/...`.
        let url = build_summary_url("https://en.wikipedia.org/api/rest_v1/", "Lyon");
        assert!(!url.contains("//page"));
        assert!(url.ends_with("/page/summary/Lyon"));
    }

    #[test]
    fn shape_payload_extracts_required_fields() {
        let body = json!({
            "type": "standard",
            "title": "Lyon",
            "displaytitle": "Lyon",
            "extract": "Lyon is a city in France.",
            "extract_html": "<p>Lyon is a city in France.</p>",
            "description": "city in France",
            "content_urls": {
                "desktop": {"page": "https://en.wikipedia.org/wiki/Lyon"},
                "mobile": {"page": "https://en.wikipedia.org/wiki/Lyon"}
            }
        });
        let payload = shape_payload(&body, "Lyon").unwrap();
        assert_eq!(payload["title"], "Lyon");
        assert_eq!(payload["description"], "city in France");
        assert_eq!(payload["extract"], "Lyon is a city in France.");
        assert_eq!(payload["url"], "https://en.wikipedia.org/wiki/Lyon");
    }

    #[test]
    fn shape_payload_falls_back_when_fields_missing() {
        // The endpoint usually returns all four fields, but
        // disambiguation and list pages omit `extract`. The shape
        // function surfaces the missing field as `""` for the
        // optional strings and surfaces a clear error for `extract`.
        let body = json!({
            "title": "Foo",
            "content_urls": {"desktop": {"page": "https://en.wikipedia.org/wiki/Foo"}}
        });
        let err = shape_payload(&body, "Foo").unwrap_err();
        match err {
            AgentError::AgentFailed(msg) => {
                assert!(msg.contains("missing `extract`"));
            }
            other => panic!("expected AgentFailed, got {other:?}"),
        }
    }

    #[test]
    fn shape_payload_handles_missing_url() {
        // `content_urls` is sometimes absent on stub pages. The URL
        // falls back to `""` so the LLM still gets a valid envelope.
        let body = json!({
            "title": "Foo",
            "extract": "A short stub.",
            "description": "stub"
        });
        let payload = shape_payload(&body, "Foo").unwrap();
        assert_eq!(payload["url"], "");
    }

    #[test]
    fn parse_args_rejects_missing_title() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WikipediaAgent::default();
        let err = rt.block_on(agent.invoke(json!({}))).unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("`title`"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_rejects_empty_title() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WikipediaAgent::default();
        let err = rt
            .block_on(agent.invoke(json!({"title": "  "})))
            .unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_oversized_title() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WikipediaAgent::default();
        let err = rt
            .block_on(agent.invoke(json!({"title": "x".repeat(MAX_TITLE_LEN + 1)})))
            .unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("at most 200"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_rejects_null_bytes() {
        // Defensive: a JSON-encoded null byte should never reach
        // the upstream, and the agent must reject it at the gate.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = WikipediaAgent::default();
        let err = rt
            .block_on(agent.invoke(json!({"title": "foo\u{0}bar"})))
            .unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("null byte"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn user_agent_default_is_descriptive() {
        // Wikimedia 403s unidentified clients. The default must
        // include the application name so operators can be
        // identified in Wikimedia's logs if they misbehave.
        let cfg = WikipediaConfig::default();
        assert!(cfg.user_agent.starts_with("nagent-wikipedia-agent/"));
        assert!(!cfg.user_agent.is_empty());
    }

    #[test]
    fn build_summary_prefers_description() {
        // When both `description` and `extract` are present, the
        // description wins (it is the shorter, headline-style label).
        let payload = json!({
            "title": "Lyon",
            "description": "city in France",
            "extract": "Lyon is a city in France. It has 2.3 million inhabitants."
        });
        let s = build_summary(&payload, "Lyon");
        assert_eq!(s, "Lyon — city in France");
    }

    #[test]
    fn build_summary_falls_back_to_extract() {
        // When `description` is empty (common on stub pages), we
        // surface the first sentence of the extract instead.
        let payload = json!({
            "title": "Foo",
            "description": "",
            "extract": "Foo is a stub article. It needs more content."
        });
        let s = build_summary(&payload, "Foo");
        assert!(s.starts_with("Foo — "), "summary: {s}");
        assert!(s.contains("stub article"), "summary: {s}");
    }
}
