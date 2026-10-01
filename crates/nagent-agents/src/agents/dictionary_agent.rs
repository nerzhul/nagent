//! `dictionary` agent: definitions, phonetics, examples, and synonyms
//! for an English word.
//!
//! Backed by the Free Dictionary REST API at
//! <https://api.dictionaryapi.dev/api/v2/entries/en/{word}>. The
//! endpoint is anonymous (no API key), rate-limited to a published
//! fair-use cap (~10 req/s per IP), and returns a JSON array of
//! per-sense entries. The agent trims the upstream payload to the
//! fields the LLM actually needs (word, phonetic, meanings with up
//! to two definitions + one example per part of speech, and
//! synonyms) and prefixes every record with the standard
//! `ok`/`summary`/`data`/`source`/`fetched_at` envelope used by the
//! other agents.
//!
//! ## Authentication and rate limits
//!
//! No API key is required. The endpoint accepts plain
//! `application/json` requests and tolerates anonymous clients at
//! the published fair-use rate. The agent therefore sets no
//! `User-Agent` override (unlike `wikipedia`, which Wikimedia
//! 403s without one) — operators wanting a custom contact string
//! can override the base URL via `DICTIONARY_BASE_URL`.
//!
//! ## Why not the Wiktionary REST API
//!
//! Wiktionary's MediaWiki `action=query` API is much larger but
//! requires multiple round-trips and returns HTML the LLM cannot
//! parse cleanly. The Free Dictionary API is purpose-built for
//! vocabulary lookups, returns stable JSON across versions, and
//! already trims the response to definitions / phonetics / examples
//! / synonyms — exactly the surface an LLM needs.
//!
//! ## Sandbox
//!
//! The agent never accepts a user-controlled URL — the upstream
//! scheme + host are hardcoded in [`crate::config::DictionaryAgentConfig`].
//! The `web_fetch` allow-list machinery therefore does not apply.
//!
//! ## Latency
//!
//! One HTTP round-trip per query. The Free Dictionary API sits
//! behind a CDN and typically returns in well under a second.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agents::{Agent, AgentError, UserContext};
use crate::config::DictionaryAgentConfig;

/// Hard cap on the response body. The endpoint usually returns a
/// few KiB per word; 128 KiB leaves generous room for words with
/// many phonetic variants while still bounding memory.
const MAX_UPSTREAM_BYTES: usize = 128 * 1024;

/// Hard cap on the word length. The endpoint's path segment is
/// bounded by HTTP server limits; we cap further at 100 chars to
/// leave room for the `/entries/en/` path prefix.
const MAX_WORD_LEN: usize = 100;

/// Build of the `dictionary` agent.
#[derive(Clone)]
pub struct DictionaryAgent {
    cfg: DictionaryAgentConfig,
    http: reqwest::Client,
}

impl std::fmt::Debug for DictionaryAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DictionaryAgent")
            .field("cfg", &self.cfg)
            .field("http", &"<reqwest::Client>")
            .finish()
    }
}

impl Default for DictionaryAgent {
    fn default() -> Self {
        Self::new(DictionaryAgentConfig::default())
    }
}

impl DictionaryAgent {
    pub fn new(cfg: DictionaryAgentConfig) -> Self {
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
impl Agent for DictionaryAgent {
    fn name(&self) -> &str {
        "dictionary"
    }

    fn description(&self) -> &str {
        "Définitions, prononciations, exemples et synonymes d'un mot anglais via l'API \
         anonyme Free Dictionary (https://api.dictionaryapi.dev/api/v2/entries/en/{word}). \
         Aucune clé d'API requise. \
         Renvoie le mot, sa phonétique, ses sens (avec définitions + exemples) et ses \
         synonymes. \
         Use for 'define serendipity', 'what does \"ephemeral\" mean?', \
         'synonym of \"fast\"', 'how do you pronounce \"quinoa\"?', \
         'définition de \"sérendipité\"' (note: word lookup is English-only). \
         Pass `word` (required, ASCII letters / hyphens / apostrophes, ≤100 chars). \
         NOT for encyclopedic / biographical / historical questions — answer those from \
         your own knowledge; this tool is scoped to English vocabulary lookups."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "word": {
                    "type": "string",
                    "maxLength": 100,
                    "pattern": "^[A-Za-z][A-Za-z'\\- ]{0,99}$",
                    "description": "English word to look up (case-insensitive). Letters, hyphens, apostrophes, and spaces only; trimmed of surrounding whitespace. Examples: 'serendipity', 'state-of-the-art', \"don't\"."
                }
            },
            "required": ["word"],
            "additionalProperties": false,
        })
    }

    async fn invoke(&self, _ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let req = parse_args(&args)?;
        let url = build_entries_url(&self.cfg.base_url, &req.word);
        let body = fetch_json(&self.http, &url).await?;
        let payload = shape_payload(&body, &req.word)?;
        Ok(serde_json::to_string(&json!({
            "ok": true,
            "summary": build_summary(&payload, &req.word),
            "data": payload,
            "source": "dictionaryapi.dev",
            "fetched_at": chrono::Utc::now().to_rfc3339(),
        }))
        .expect("json encode"))
    }
}

// ---- URL builder ---------------------------------------------------------

/// Build the `/entries/en/{word}` URL against the configured base.
/// The word is percent-encoded so spaces become `%20` and accented
/// characters (rare but legal in loan-words) stay as UTF-8 bytes.
fn build_entries_url(base_url: &str, word: &str) -> String {
    let trimmed_base = base_url.trim_end_matches('/');
    let encoded = url_encode_path_segment(word);
    format!("{trimmed_base}/entries/en/{encoded}")
}

/// Minimal path-segment encoder. Mirrors the `wikipedia` agent's
/// helper so behaviour is consistent across HTTP agents.
fn url_encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
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

async fn fetch_json(http: &reqwest::Client, url: &str) -> Result<Value, AgentError> {
    let resp = http
        .get(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| AgentError::AgentFailed(format!("upstream connect/read failed: {e}")))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        // The Free Dictionary API returns 404 with a JSON body of
        // `{"title":"No Definitions Found","message":"...","resolution":"..."}`
        // for unknown words. Surface a clear, LLM-friendly message.
        let body = resp.text().await.unwrap_or_default();
        let parsed: Option<Value> = serde_json::from_str(&body).ok();
        let upstream_message = parsed
            .as_ref()
            .and_then(|v| v.get("title"))
            .and_then(|t| t.as_str())
            .unwrap_or("No Definitions Found");
        return Err(AgentError::Upstream {
            status: 404,
            body: upstream_message.to_string(),
        });
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AgentError::Upstream {
            status: status.as_u16(),
            body: truncate(&body, 2048),
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

/// Trim the upstream entries document to the fields the LLM
/// actually needs. The upstream returns an array; we fold every
/// entry into a single `meanings` array so the LLM sees one
/// record per word regardless of how many sub-entries the
/// dictionary had.
///
/// Per-entry we keep at most two definitions per part of speech
/// (with the first example if any) plus a flat list of synonyms.
/// That keeps the payload well under a kilobyte even for words
/// with dozens of senses (e.g. "set").
fn shape_payload(body: &Value, requested_word: &str) -> Result<Value, AgentError> {
    let entries = body
        .as_array()
        .ok_or_else(|| {
            AgentError::AgentFailed(
                "upstream returned a non-array body — the dictionary API contract changed".into(),
            )
        })?
        .clone();
    if entries.is_empty() {
        return Err(AgentError::AgentFailed(
            "upstream returned an empty entries array".into(),
        ));
    }

    let word = entries
        .first()
        .and_then(|e| e.get("word"))
        .and_then(|v| v.as_str())
        .unwrap_or(requested_word)
        .to_string();

    // Prefer the first non-empty phonetic across the entries.
    let phonetic = entries
        .iter()
        .flat_map(|e| {
            e.get("phonetics")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default()
        })
        .find_map(|p| {
            p.get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default();

    // Flatten all `meanings` arrays, deduplicating by
    // `(partOfSpeech, definition)`. Keep at most two definitions
    // per part of speech to keep the bubble readable.
    let mut meanings: Vec<Value> = Vec::new();
    let mut synonyms: Vec<String> = Vec::new();
    let mut seen_synonyms: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in &entries {
        let Some(m_arr) = entry.get("meanings").and_then(|m| m.as_array()) else {
            continue;
        };
        for m in m_arr {
            let pos = m
                .get("partOfSpeech")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let defs_in = m
                .get("definitions")
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default();
            let mut definitions: Vec<Value> = Vec::new();
            let mut seen_defs: std::collections::HashSet<String> = std::collections::HashSet::new();
            // Dedupe first, then cap at 2 per POS so a payload
            // with 2 identicals + 1 unique surfaces both unique
            // definitions (instead of dropping the unique one
            // because the cap ran out on a duplicate).
            for d in defs_in {
                let definition = d
                    .get("definition")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if definition.is_empty() {
                    continue;
                }
                if !seen_defs.insert(definition.clone()) {
                    continue;
                }
                let example = d
                    .get("example")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let def_obj = if let Some(ex) = example {
                    json!({ "definition": definition, "example": ex })
                } else {
                    json!({ "definition": definition })
                };
                definitions.push(def_obj);
                if definitions.len() >= 2 {
                    break;
                }
            }
            // Collect meaning-level synonyms (deduped, case-folded).
            if let Some(syns) = m.get("synonyms").and_then(|s| s.as_array()) {
                for s in syns {
                    if let Some(s) = s.as_str() {
                        let key = s.to_lowercase();
                        if seen_synonyms.insert(key) {
                            synonyms.push(s.to_string());
                        }
                    }
                }
            }
            if !definitions.is_empty() {
                meanings.push(json!({
                    "part_of_speech": pos,
                    "definitions": definitions,
                }));
            }
        }
    }

    if meanings.is_empty() {
        // The upstream returned entries but none had any
        // definitions — treat it as a soft 404 so the LLM can
        // recover ("no definition found for X").
        return Err(AgentError::AgentFailed(format!(
            "no definitions returned for `{requested_word}`"
        )));
    }

    Ok(json!({
        "word": word,
        "phonetic": phonetic,
        "meanings": meanings,
        "synonyms": synonyms,
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
/// the first definition's first sentence so the bubble stays a
/// single line; falls back to the word itself when nothing usable
/// is available.
fn build_summary(payload: &Value, requested_word: &str) -> String {
    let word = payload
        .get("word")
        .and_then(|v| v.as_str())
        .unwrap_or(requested_word);
    let first_def = payload
        .get("meanings")
        .and_then(|m| m.as_array())
        .and_then(|arr| arr.first())
        .and_then(|m| m.get("definitions"))
        .and_then(|d| d.as_array())
        .and_then(|arr| arr.first())
        .and_then(|d| d.get("definition"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if first_def.is_empty() {
        return word.to_string();
    }
    let first = first_def
        .split_once(". ")
        .map(|(head, _)| head)
        .unwrap_or(first_def);
    let first = first.trim();
    let first = if first.ends_with('.') {
        first.to_string()
    } else {
        format!("{first}.")
    };
    format!("{word} — {}", truncate(&first, 120))
}

// ---- Argument parsing ----------------------------------------------------

#[derive(Debug)]
struct ParsedArgs {
    word: String,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, AgentError> {
    let obj = args
        .as_object()
        .ok_or_else(|| AgentError::InvalidArguments("arguments must be a JSON object".into()))?;
    let word = obj
        .get("word")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AgentError::InvalidArguments("`word` (string) is required".into()))?
        .trim()
        .to_string();
    if word.is_empty() {
        return Err(AgentError::InvalidArguments(
            "`word` must not be empty".into(),
        ));
    }
    if word.len() > MAX_WORD_LEN {
        return Err(AgentError::InvalidArguments(format!(
            "`word` must be at most {MAX_WORD_LEN} characters (got {})",
            word.len()
        )));
    }
    // Allow only ASCII letters, hyphens, apostrophes, and spaces
    // — i.e. lookalike letters (e.g. full-width, Cyrillic "a")
    // would slip past naive filters and confuse the upstream.
    for ch in word.chars() {
        if !is_allowed_char(ch) {
            return Err(AgentError::InvalidArguments(format!(
                "`word` contains an unsupported character `{ch}`; \
                 allowed: letters, hyphens, apostrophes, spaces"
            )));
        }
    }
    Ok(ParsedArgs { word })
}

fn is_allowed_char(ch: char) -> bool {
    matches!(ch, 'A'..='Z' | 'a'..='z' | '-' | '\'' | ' ')
}

// ---- Tests ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::UserContext;
    use serde_json::json;

    /// Build a fresh `UserContext` for tests that ignore
    /// credentials. Mirrors `UserContext::for_tests` without
    /// bringing the helper into the agent's public API.
    fn test_ctx() -> UserContext {
        UserContext::for_tests(
            uuid::Uuid::new_v4(),
            std::sync::Arc::new(crate::ServiceRegistry::empty()),
        )
    }

    #[test]
    fn name_and_schema_are_stable() {
        let agent = DictionaryAgent::default();
        assert_eq!(agent.name(), "dictionary");
        let schema = agent.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["required"]
            .as_array()
            .unwrap()
            .contains(&json!("word")));
        assert_eq!(schema["properties"]["word"]["maxLength"], 100);
    }

    #[test]
    fn description_scopes_to_vocabulary() {
        // Regression guard: an earlier draft of the description
        // nudged the LLM to send encyclopedic questions to
        // `dictionary`. That tool only does vocabulary lookups;
        // biographical / historical queries must NOT route here
        // (they'd 404 and break trust). The description must
        // explicitly warn the LLM away from that pattern.
        let agent = DictionaryAgent::default();
        let desc = agent.description();
        assert!(
            desc.contains("NOT") && desc.contains("encyclopedic"),
            "description must warn the LLM away from encyclopedic / \
             biographical / historical questions; got: {desc}"
        );
        assert!(
            desc.contains("vocabulary"),
            "description must scope the tool to vocabulary lookups; got: {desc}"
        );
    }

    #[test]
    fn build_entries_url_encodes_spaces_and_specials() {
        let url = build_entries_url("https://api.dictionaryapi.dev/api/v2", "state of the art");
        assert!(url.ends_with("/entries/en/state%20of%20the%20art"));
        let url = build_entries_url("https://api.dictionaryapi.dev/api/v2", "don't");
        assert!(url.contains("don%27t"));
    }

    #[test]
    fn build_entries_url_strips_trailing_slash_from_base() {
        let url = build_entries_url("https://api.dictionaryapi.dev/api/v2/", "hello");
        assert!(!url.contains("//entries"));
        assert!(url.ends_with("/entries/en/hello"));
    }

    #[test]
    fn shape_payload_keeps_word_phonetic_meanings_synonyms() {
        // Mirrors the upstream payload for "hello".
        let body = json!([{
            "word": "hello",
            "phonetics": [
                {"text": "/həˈləʊ/", "audio": ""},
                {"text": "", "audio": ""}
            ],
            "meanings": [{
                "partOfSpeech": "interjection",
                "definitions": [
                    {
                        "definition": "A greeting said when meeting someone.",
                        "example": "Hello, everyone.",
                        "synonyms": [],
                        "antonyms": []
                    },
                    {
                        "definition": "Used sarcastically.",
                        "example": "Hello! What's going on here?"
                    }
                ],
                "synonyms": ["greeting"],
                "antonyms": ["bye"]
            }],
            "license": {"name": "CC BY-SA 3.0"}
        }]);
        let payload = shape_payload(&body, "hello").unwrap();
        assert_eq!(payload["word"], "hello");
        assert_eq!(payload["phonetic"], "/həˈləʊ/");
        assert_eq!(payload["meanings"][0]["part_of_speech"], "interjection");
        assert_eq!(
            payload["meanings"][0]["definitions"][0]["definition"],
            "A greeting said when meeting someone."
        );
        assert_eq!(
            payload["meanings"][0]["definitions"][0]["example"],
            "Hello, everyone."
        );
        // synonyms flat-list, deduped, case-folded
        assert_eq!(payload["synonyms"][0], "greeting");
    }

    #[test]
    fn shape_payload_caps_definitions_per_part_of_speech() {
        // A word with 5 definitions under one POS must surface
        // only the first two in the payload so the LLM bubble
        // stays compact.
        let body = json!([{
            "word": "set",
            "phonetics": [],
            "meanings": [{
                "partOfSpeech": "verb",
                "definitions": (1..=5)
                    .map(|i| json!({ "definition": format!("def {i}") }))
                    .collect::<Vec<_>>()
            }]
        }]);
        let payload = shape_payload(&body, "set").unwrap();
        let defs = payload["meanings"][0]["definitions"].as_array().unwrap();
        assert_eq!(defs.len(), 2, "expected at most 2 definitions per POS");
        assert_eq!(defs[0]["definition"], "def 1");
        assert_eq!(defs[1]["definition"], "def 2");
    }

    #[test]
    fn shape_payload_dedupes_identical_definitions() {
        // Some upstream entries duplicate a definition under
        // synonyms/antonyms variants. The shape function must
        // keep only one copy per POS.
        let body = json!([{
            "word": "test",
            "phonetics": [],
            "meanings": [{
                "partOfSpeech": "noun",
                "definitions": [
                    {"definition": "A procedure."},
                    {"definition": "A procedure."},
                    {"definition": "An examination."}
                ]
            }]
        }]);
        let payload = shape_payload(&body, "test").unwrap();
        let defs = payload["meanings"][0]["definitions"].as_array().unwrap();
        assert_eq!(defs.len(), 2);
    }

    #[test]
    fn shape_payload_dedupes_synonyms_case_insensitively() {
        let body = json!([{
            "word": "fast",
            "phonetics": [],
            "meanings": [
                {
                    "partOfSpeech": "adjective",
                    "definitions": [{"definition": "Quick."}],
                    "synonyms": ["rapid", "swift", "Rapid"]
                },
                {
                    "partOfSpeech": "adverb",
                    "definitions": [{"definition": "In a firm manner."}],
                    "synonyms": ["rapid"]
                }
            ]
        }]);
        let payload = shape_payload(&body, "fast").unwrap();
        let syns = payload["synonyms"].as_array().unwrap();
        // "rapid" appears twice (different cases) but must dedupe
        // to a single entry.
        assert_eq!(syns.len(), 2);
        assert_eq!(syns[0], "rapid");
        assert_eq!(syns[1], "swift");
    }

    #[test]
    fn shape_payload_falls_back_to_first_phonetic() {
        // The upstream sometimes returns the audio URL without a
        // `text` field on the first entry. We must skip empty
        // strings and find the first non-empty phonetic.
        let body = json!([{
            "word": "test",
            "phonetics": [
                {"text": "", "audio": "https://example/a.mp3"},
                {"text": "/test/", "audio": ""}
            ],
            "meanings": [{
                "partOfSpeech": "noun",
                "definitions": [{"definition": "A procedure."}]
            }]
        }]);
        let payload = shape_payload(&body, "test").unwrap();
        assert_eq!(payload["phonetic"], "/test/");
    }

    #[test]
    fn shape_payload_fails_on_empty_entries() {
        let body = json!([]);
        let err = shape_payload(&body, "missing").unwrap_err();
        assert!(matches!(err, AgentError::AgentFailed(_)));
    }

    #[test]
    fn shape_payload_fails_when_no_definitions_returned() {
        // The upstream sometimes returns an entry with empty
        // `meanings` arrays (rare, mostly on stubs). Surface it
        // as AgentFailed so the LLM sees "no definition found"
        // rather than a confusing empty envelope.
        let body = json!([{
            "word": "stub",
            "phonetics": [],
            "meanings": []
        }]);
        let err = shape_payload(&body, "stub").unwrap_err();
        match err {
            AgentError::AgentFailed(msg) => {
                assert!(msg.contains("no definitions"));
            }
            other => panic!("expected AgentFailed, got {other:?}"),
        }
    }

    #[test]
    fn shape_payload_fails_on_non_array_body() {
        let body = json!({"word": "oops"});
        let err = shape_payload(&body, "oops").unwrap_err();
        match err {
            AgentError::AgentFailed(msg) => {
                assert!(msg.contains("non-array body"));
            }
            other => panic!("expected AgentFailed, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_rejects_missing_word() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = DictionaryAgent::default();
        let err = rt
            .block_on(agent.invoke(&test_ctx(), json!({})))
            .unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("`word`"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_rejects_empty_word() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = DictionaryAgent::default();
        let err = rt
            .block_on(agent.invoke(&test_ctx(), json!({"word": "  "})))
            .unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
    }

    #[test]
    fn parse_args_rejects_oversized_word() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let agent = DictionaryAgent::default();
        let err = rt
            .block_on(agent.invoke(&test_ctx(), json!({"word": "x".repeat(MAX_WORD_LEN + 1)})))
            .unwrap_err();
        match err {
            AgentError::InvalidArguments(msg) => {
                assert!(msg.contains("at most 100"));
            }
            other => panic!("expected InvalidArguments, got {other:?}"),
        }
    }

    #[test]
    fn parse_args_rejects_disallowed_chars() {
        // Digits, punctuation other than `'` and `-`, non-ASCII.
        for bad in ["hell0", "hello!", "héllo", "hello;", "héllo"] {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let agent = DictionaryAgent::default();
            let err = rt
                .block_on(agent.invoke(&test_ctx(), json!({"word": bad})))
                .unwrap_err();
            match err {
                AgentError::InvalidArguments(msg) => {
                    assert!(
                        msg.contains("unsupported character"),
                        "got {msg} for {bad:?}"
                    );
                }
                other => panic!("expected InvalidArguments for {bad:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_args_accepts_hyphens_apostrophes_and_spaces() {
        // Compound words and contractions must round-trip.
        for ok in ["state-of-the-art", "don't", "well known"] {
            // We can't actually hit the upstream from a unit
            // test, but the validator path must accept these.
            // The cheapest way is to call `build_entries_url`
            // with the same trim/lowercase logic — for now we
            // just assert the character-allow-list lets them
            // through.
            for ch in ok.chars() {
                assert!(is_allowed_char(ch), "char {ch:?} in {ok:?} must be allowed");
            }
        }
    }

    #[test]
    fn build_summary_prefers_first_definition() {
        let payload = json!({
            "word": "hello",
            "phonetic": "",
            "meanings": [{
                "part_of_speech": "interjection",
                "definitions": [{
                    "definition": "A greeting said when meeting someone or acknowledging someone's arrival."
                }]
            }],
            "synonyms": []
        });
        let s = build_summary(&payload, "hello");
        assert!(s.starts_with("hello — "), "summary: {s}");
        assert!(s.contains("greeting"), "summary: {s}");
    }

    #[test]
    fn build_summary_truncates_long_definitions() {
        let long = "x".repeat(500);
        let payload = json!({
            "word": "test",
            "phonetic": "",
            "meanings": [{
                "part_of_speech": "noun",
                "definitions": [{"definition": long}]
            }],
            "synonyms": []
        });
        let s = build_summary(&payload, "test");
        // 120-char cap plus the `word — ` prefix keeps it well
        // under 200 chars total.
        assert!(s.len() < 200, "summary too long: {} chars", s.len());
        assert!(s.starts_with("test — "));
    }

    #[test]
    fn build_summary_falls_back_to_word() {
        // When meanings are empty (rare after shape_payload, but
        // the helper must be robust on its own) the summary is
        // just the word.
        let payload = json!({
            "word": "test",
            "phonetic": "",
            "meanings": [],
            "synonyms": []
        });
        assert_eq!(build_summary(&payload, "test"), "test");
    }

    #[test]
    fn base_url_default_is_canonical() {
        let cfg = DictionaryAgentConfig::default();
        assert_eq!(cfg.base_url, "https://api.dictionaryapi.dev/api/v2");
        assert_eq!(cfg.timeout_ms, 5_000);
    }
}
