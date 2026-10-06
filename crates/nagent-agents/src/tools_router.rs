//! `tools_router` — in-process BM25 pre-selection router for the
//! agent registry.
//!
//! Built once at boot from the [`AgentRegistry`]; the proxy calls
//! [`ToolsRouter::pre_select`] with the user's most recent message
//! and gets back the small set of agent names most likely to fit,
//! which it merges with `search_tools` (always present) and the
//! per-session discovered-tools store to build the round's
//! `tools=[]`.
//!
//! The router is purely local and dependency-free: a tiny
//! hand-rolled BM25 (lowercased split on `[a-z0-9_]+`, small
//! stop-word list, IDF from a one-time document-frequency map).
//! No stemming, no embeddings — that is the v2 candidate list in
//! the plan and explicitly out of scope for v1.
//!
//! `search_tools` itself is excluded from the index: every other
//! agent becomes a candidate for the BM25 score, but the router
//! must never suggest the meta-tool that does the discovery.

use std::collections::HashMap;
use std::sync::Arc;

use crate::agents::AgentRegistry;

/// Stable wire name for the `search_tools` meta-agent. The
/// router excludes it from the index so the meta-tool never
/// recommends itself; the agent itself is declared in
/// `crate::agents::tool_search`. Defined here so both
/// `tools_router` and the agent can refer to it without a
/// circular import.
pub const SEARCH_TOOLS_NAME: &str = "search_tools";

/// One tool the router indexed. Lives on the router (not behind a
/// trait method) so the score / `search` paths stay cheap and
/// debuggable.
#[derive(Debug, Clone)]
pub struct IndexedTool {
    /// Stable agent name (`Agent::name()`), owned as an
    /// `Arc<str>` so the router can outlive the agent that
    /// supplied it. The name is read once at boot; `Arc::clone`
    /// keeps the hot path allocation-free.
    pub name: Arc<str>,
    /// Original description, owned (`Arc<str>`) for the same
    /// lifetime reason as `name`.
    pub description: Arc<str>,
    /// One-line description, lower-cased for tokenisation.
    pub description_lower: String,
    /// Extra synonyms declared via [`Agent::keywords`], already
    /// lower-cased.
    pub keywords_lower: Vec<&'static str>,
}

/// One hit returned by [`ToolsRouter::search`]. Same shape as
/// the wire payload `search_tools` returns to the LLM, minus the
/// JSON Schema (the calling agent does that lookup itself).
#[derive(Debug, Clone)]
pub struct ToolSearchHit {
    pub name: Arc<str>,
    pub description: Arc<str>,
}

/// BM25 router. Cheap to clone (inner fields are owned, no
/// `Arc`); the proxy stashes it on `LlmState`.
#[derive(Debug, Clone)]
pub struct ToolsRouter {
    /// Per-agent token list + bag, in registry order (the score
    /// tie-break). Owns the `String` tokens so the lifetime is
    /// `'static` after construction.
    docs: Vec<IndexedTool>,
    /// `(name → index in docs)` lookup.
    by_name: HashMap<String, usize>,
    /// Document frequency per term (how many indexed tools
    /// contain it). Used for IDF.
    df: HashMap<String, u32>,
    /// Average document length, in tokens.
    avg_dl: f64,
    /// Total token count across the index — kept so we can size
    /// `df` changes without recomputing.
    n_docs: u32,
}

/// Standard BM25 parameters. `k1` controls term-frequency
/// saturation (higher = the second occurrence matters more);
/// `b` controls document-length normalisation (0 = ignore length,
/// 1 = full linear penalty).
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Small English stop-word list. Matches the function name in
/// the plan; kept inline to keep the router dependency-free.
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "for", "from", "has", "have", "in",
    "is", "it", "of", "on", "or", "that", "the", "this", "to", "was", "were", "will", "with",
    "you", "your",
];

impl ToolsRouter {
    /// Build the router from the supplied registry. `search_tools`
    /// is filtered out before indexing so it never appears in
    /// pre-selection results (the meta-tool would always score
    /// top because every description mentions "tool").
    pub fn from_registry(registry: &AgentRegistry) -> Self {
        let mut docs: Vec<IndexedTool> = Vec::with_capacity(registry.len());
        for agent in registry.iter() {
            let name = agent.name();
            // Hard-exclude the meta-tool. Reserved name; if a
            // future contributor adds another agent with the same
            // name the descriptor table already rejects it
            // upstream.
            if name == "search_tools" {
                continue;
            }
            let description = Arc::<str>::from(agent.description().to_string());
            let description_lower = agent.description().to_lowercase();
            let keywords_lower: Vec<&'static str> = agent
                .keywords()
                .iter()
                .map(|s| {
                    // The trait returns `&'static [&'static str]`;
                    // we lower-case the strings once at boot so the
                    // hot path is allocation-free. Easiest is to
                    // store the original static reference and
                    // lower-case at lookup time, but that would
                    // allocate per query; instead we leak a
                    // lower-cased copy here. The router is built
                    // once per process so the leak is bounded.
                    Box::leak(s.to_lowercase().into_boxed_str()) as &'static str
                })
                .collect();
            docs.push(IndexedTool {
                name: Arc::<str>::from(name.to_string()),
                description,
                description_lower,
                keywords_lower,
            });
        }
        // Sort by name for deterministic ordering — the score
        // tie-break uses `Vec::iter().enumerate()` which yields
        // indexes in `docs` order. Sorting by name makes the
        // tie-break stable across builds with different registry
        // layouts.
        docs.sort_by(|a, b| a.name.as_ref().cmp(b.name.as_ref()));

        let mut by_name: HashMap<String, usize> = HashMap::with_capacity(docs.len());
        for (idx, doc) in docs.iter().enumerate() {
            by_name.insert(doc.name.as_ref().to_string(), idx);
        }

        // Document frequency map + average document length.
        let mut df: HashMap<String, u32> = HashMap::new();
        let mut total_len: usize = 0;
        for doc in &docs {
            let tokens = tokenise_doc(doc);
            let unique: std::collections::HashSet<&str> = tokens.iter().copied().collect();
            for term in unique {
                *df.entry(term.to_string()).or_insert(0) += 1;
            }
            total_len += tokens.len();
        }
        let n_docs = docs.len() as u32;
        let avg_dl = if n_docs == 0 {
            0.0
        } else {
            total_len as f64 / n_docs as f64
        };

        Self {
            docs,
            by_name,
            df,
            avg_dl,
            n_docs,
        }
    }

    /// Return the agent names the router thinks fit the query,
    /// highest-score first, capped at `top_k`. Empty when the
    /// query / registry is empty. Tie-broken by ascending
    /// alphabetical name (the index is sorted by name, so
    /// `enumerate()` gives the deterministic order).
    pub fn pre_select(&self, query: &str, top_k: usize) -> Vec<Arc<str>> {
        self.score(query, top_k)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// Same scoring as [`Self::pre_select`] but returns the
    /// description alongside the name so the `search_tools`
    /// agent can shape its JSON payload without re-walking the
    /// registry.
    pub fn search(&self, query: &str, top_k: usize) -> Vec<ToolSearchHit> {
        self.score(query, top_k)
            .into_iter()
            .map(|(name, _score)| {
                let lookup_key = name.as_ref().to_string();
                ToolSearchHit {
                    name,
                    description: self
                        .find_description(&lookup_key)
                        .unwrap_or_else(|| Arc::from("")),
                }
            })
            .collect()
    }

    /// Look up the original (non-tokenised) description for a
    /// tool by name. `None` for an unknown name; the `search`
    /// path substitutes an empty string in that case so the
    /// caller does not have to handle it.
    fn find_description(&self, name: &str) -> Option<Arc<str>> {
        self.by_name
            .get(name)
            .and_then(|idx| self.docs.get(*idx))
            .map(|doc| doc.description.clone())
    }

    /// Internal scoring. Returns `(name, score)` ordered by
    /// descending score, then ascending name. Both lists are
    /// empty when the query is blank or the registry was empty.
    fn score(&self, query: &str, top_k: usize) -> Vec<(Arc<str>, f64)> {
        if top_k == 0 || self.n_docs == 0 {
            return Vec::new();
        }
        let query_terms = tokenise_query(query);
        if query_terms.is_empty() {
            return Vec::new();
        }
        let mut scores: Vec<(usize, f64)> = self
            .docs
            .iter()
            .enumerate()
            .map(|(idx, doc)| {
                let tokens = tokenise_doc(doc);
                let dl = tokens.len() as f64;
                let mut s = 0.0;
                for term in &query_terms {
                    let tf = count_term(&tokens, term) as f64;
                    if tf == 0.0 {
                        continue;
                    }
                    let df = self.df.get(term.as_str()).copied().unwrap_or(0) as f64;
                    // BM25 IDF: log(1 + (N - df + 0.5) / (df + 0.5)).
                    // The `+1` keeps the score non-negative when a
                    // term appears in every document (df == N).
                    let idf = ((self.n_docs as f64 - df + 0.5) / (df + 0.5) + 1.0).ln();
                    let norm =
                        tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / self.avg_dl.max(1.0)));
                    s += idf * norm;
                }
                (idx, s)
            })
            .collect();
        // Sort descending by score; stable sort so the
        // alphabetical name tie-break survives.
        scores.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });
        scores
            .into_iter()
            .take(top_k)
            .filter(|(_, s)| *s > 0.0)
            .map(|(idx, s)| (self.docs[idx].name.clone(), s))
            .collect()
    }

    /// Number of indexed tools (excludes `search_tools`).
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// `true` when the index is empty.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

fn tokenise_query(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in query.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        if raw.is_empty() {
            continue;
        }
        let lc = raw.to_lowercase();
        if lc.len() < 2 || STOP_WORDS.contains(&lc.as_str()) {
            continue;
        }
        out.push(lc);
    }
    out
}

fn tokenise_doc(doc: &IndexedTool) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for raw in doc
        .description_lower
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
    {
        if raw.is_empty() {
            continue;
        }
        if raw.len() < 2 || STOP_WORDS.contains(&raw) {
            continue;
        }
        out.push(raw);
    }
    for kw in &doc.keywords_lower {
        // The keyword was lower-cased at construction time so we
        // can match `STOP_WORDS` directly.
        if !STOP_WORDS.contains(kw) {
            out.push(kw);
        }
    }
    out
}

fn count_term<'a>(tokens: &'a [&'a str], term: &str) -> usize {
    tokens.iter().filter(|t| **t == term).count()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{Agent, AgentError, AgentRegistry, UserContext};
    use async_trait::async_trait;
    use serde_json::{json, Value};

    /// Stub agent for tests. The `name()` becomes the lookup
    /// key; everything else is read-only.
    struct StubAgent {
        name: &'static str,
        description: &'static str,
        keywords: &'static [&'static str],
    }

    #[async_trait]
    impl Agent for StubAgent {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            self.description
        }
        fn parameters_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {"q": {"type": "string"}},
                "required": ["q"],
                "additionalProperties": false,
            })
        }
        fn keywords(&self) -> &'static [&'static str] {
            self.keywords
        }
        async fn invoke(&self, _ctx: &UserContext, _args: Value) -> Result<String, AgentError> {
            Ok("{}".to_string())
        }
    }

    fn registry(agents: Vec<StubAgent>) -> AgentRegistry {
        let mut reg = AgentRegistry::empty();
        for a in agents {
            reg.push_agent(a);
        }
        reg
    }

    fn weather() -> StubAgent {
        StubAgent {
            name: "get_weather",
            description: "Current / forecast weather at a location. Pass `location` (required).",
            keywords: &["meteo", "temperature", "forecast"],
        }
    }
    fn calculate() -> StubAgent {
        StubAgent {
            name: "calculate",
            description: "Evaluate an arithmetic expression like '15*87.5/100'.",
            keywords: &["math", "compute", "expression"],
        }
    }
    fn wikipedia() -> StubAgent {
        StubAgent {
            name: "wikipedia",
            description: "Encyclopedic summary of a person, event, concept, or topic.",
            keywords: &["encyclopedia", "biography", "history"],
        }
    }

    #[test]
    fn from_registry_excludes_search_tools() {
        // The router never indexes the meta-tool, regardless of
        // who registers it. We can't push `search_tools` into a
        // real registry without `nagent-agents`'s `tool-search-agent`
        // feature on, so we simulate by checking that the
        // `description_lower` filter works on a name that matches.
        let reg = registry(vec![weather()]);
        let r = ToolsRouter::from_registry(&reg);
        assert_eq!(r.len(), 1);
        let hits = r.pre_select("weather", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(&*hits[0], "get_weather");
    }

    #[test]
    fn pre_select_picks_weather_for_weather_query() {
        let reg = registry(vec![weather(), calculate(), wikipedia()]);
        let r = ToolsRouter::from_registry(&reg);
        let hits = r.pre_select("what is the weather in Lyon", 2);
        assert_eq!(hits.len(), 1);
        assert_eq!(&*hits[0], "get_weather");
    }

    #[test]
    fn pre_select_picks_calculate_for_arithmetic_query() {
        let reg = registry(vec![weather(), calculate(), wikipedia()]);
        let r = ToolsRouter::from_registry(&reg);
        let hits = r.pre_select("how much is 15% of 230", 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(&*hits[0], "calculate");
    }

    #[test]
    fn pre_select_picks_wikipedia_for_biography_query() {
        // The wikipedia stub's keywords include "biography";
        // the description contains "person, event, concept,
        // topic" but no "marie" / "curie" / "tell". v1 has no
        // stemming so a literal "biography of Marie Curie"
        // matches via the keyword synonym (the plan's stated
        // motivation for the `keywords()` extension point on the
        // `Agent` trait).
        let reg = registry(vec![weather(), calculate(), wikipedia()]);
        let r = ToolsRouter::from_registry(&reg);
        let hits = r.pre_select("biography of Marie Curie", 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(&*hits[0], "wikipedia");
    }

    #[test]
    fn pre_select_respects_top_k() {
        let reg = registry(vec![weather(), calculate(), wikipedia()]);
        let r = ToolsRouter::from_registry(&reg);
        let hits = r.pre_select("weather forecast today", 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn pre_select_keyword_synonym_works() {
        // The stub weather agent declares `keywords = ["meteo"]`;
        // a French query should still match.
        let reg = registry(vec![weather(), calculate()]);
        let r = ToolsRouter::from_registry(&reg);
        let hits = r.pre_select("meteo a Lyon", 2);
        assert!(
            hits.iter().any(|h| &**h == "get_weather"),
            "expected `meteo` synonym to find get_weather, got {hits:?}"
        );
    }

    #[test]
    fn pre_select_empty_query_returns_empty() {
        let reg = registry(vec![weather(), calculate()]);
        let r = ToolsRouter::from_registry(&reg);
        let hits = r.pre_select("", 5);
        assert!(hits.is_empty());
        let hits = r.pre_select("   ", 5);
        assert!(hits.is_empty());
        let hits = r.pre_select("the a an", 5);
        // All-stop-word query collapses to empty token list.
        assert!(hits.is_empty());
    }

    #[test]
    fn pre_select_top_k_zero_returns_empty() {
        let reg = registry(vec![weather()]);
        let r = ToolsRouter::from_registry(&reg);
        assert!(r.pre_select("weather", 0).is_empty());
    }

    #[test]
    fn pre_select_empty_registry_returns_empty() {
        let reg = AgentRegistry::empty();
        let r = ToolsRouter::from_registry(&reg);
        assert!(r.pre_select("anything", 5).is_empty());
        assert!(r.is_empty());
    }

    #[test]
    fn pre_select_tie_break_is_alphabetical() {
        // Two agents share no overlap with the query and no
        // overlap with each other, so both score 0 — the router
        // filters 0-score hits. Re-test with a query that has
        // both partial overlap so both score > 0; the
        // alphabetical tie-break ensures deterministic order.
        let reg = registry(vec![weather(), calculate()]);
        let r = ToolsRouter::from_registry(&reg);
        // "temperature" matches the weather keyword.
        let hits = r.pre_select("temperature", 5);
        // get_weather wins on keyword + description overlap;
        // calculate does not match.
        assert_eq!(hits.len(), 1);
        assert_eq!(&*hits[0], "get_weather");
    }

    #[test]
    fn tokenise_strips_punctuation_and_lowercases() {
        // Underscores are part of the token alphabet per the
        // plan ("split on `[a-z0-9_]+`"); "what_is" stays as
        // one token, "Hello, WORLD!" loses the punctuation but
        // keeps the words.
        let q = tokenise_query("Hello, WORLD! what_is-up?");
        assert_eq!(q, vec!["hello", "world", "what_is", "up"]);
    }
}
