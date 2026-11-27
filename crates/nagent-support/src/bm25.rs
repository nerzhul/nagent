//! Tiny dependency-free BM25 ranker, shared by the agent
//! pre-selection router (`nagent-agents::tools_router`) and by
//! callers that need to rank free-text documents locally (e.g.
//! `nagent-shortcut`'s automatic epic classification).
//!
//! Same recipe as the router: lowercased split on
//! `[a-z0-9_]+`, a small stop-word list, IDF from a one-time
//! document-frequency map. No stemming, no embeddings.

use std::collections::HashMap;

/// Standard BM25 parameters. `K1` controls term-frequency
/// saturation; `B` controls document-length normalisation.
pub const K1: f64 = 1.2;
pub const B: f64 = 0.75;

/// Small English stop-word list (same list as the tools router).
pub const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "do", "for", "from", "has", "have", "in",
    "is", "it", "of", "on", "or", "that", "the", "this", "to", "was", "were", "will", "with",
    "you", "your",
];

/// Tokenise free text: split on non-`[a-z0-9_]`, lowercase, drop
/// 1-char tokens and stop words.
pub fn tokenise(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
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

fn count_term(tokens: &[String], term: &str) -> usize {
    tokens.iter().filter(|t| t.as_str() == term).count()
}

/// Score `docs` against `query` with BM25. Returns
/// `(doc_index, score)` pairs sorted by descending score, then
/// ascending index; entries with a zero score are dropped. Empty
/// query or empty doc list yields an empty vector.
pub fn bm25_scores(query: &str, docs: &[String]) -> Vec<(usize, f64)> {
    let query_terms = tokenise(query);
    let n_docs = docs.len();
    if query_terms.is_empty() || n_docs == 0 {
        return Vec::new();
    }
    let tokenised: Vec<Vec<String>> = docs.iter().map(|d| tokenise(d)).collect();
    let avg_dl = tokenised.iter().map(|t| t.len()).sum::<usize>() as f64 / n_docs as f64;

    // Document frequency per term, for IDF.
    let mut df: HashMap<&str, u32> = HashMap::new();
    for tokens in &tokenised {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for t in tokens {
            if seen.insert(t.as_str()) {
                *df.entry(t.as_str()).or_insert(0) += 1;
            }
        }
    }

    let mut scores: Vec<(usize, f64)> = tokenised
        .iter()
        .enumerate()
        .map(|(idx, tokens)| {
            let dl = tokens.len() as f64;
            let mut s = 0.0;
            for term in &query_terms {
                let tf = count_term(tokens, term) as f64;
                if tf == 0.0 {
                    continue;
                }
                let df_t = df.get(term.as_str()).copied().unwrap_or(0) as f64;
                // BM25 IDF: ln(1 + (N - df + 0.5) / (df + 0.5)).
                // The `+1` inside the log keeps the score
                // non-negative when a term appears in every
                // document (df == N).
                let idf = ((n_docs as f64 - df_t + 0.5) / (df_t + 0.5) + 1.0).ln();
                let norm = tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avg_dl.max(1.0)));
                s += idf * norm;
            }
            (idx, s)
        })
        .collect();
    scores.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scores.into_iter().filter(|(_, s)| *s > 0.0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenise_lowercases_splits_and_drops_stopwords() {
        assert_eq!(
            tokenise("Crash au Login, the app!"),
            vec![
                "crash".to_string(),
                "au".to_string(),
                "login".to_string(),
                "app".to_string()
            ]
        );
        assert!(tokenise("the a I").is_empty());
    }

    #[test]
    fn empty_query_or_docs_scores_nothing() {
        assert!(bm25_scores("", &["doc".to_string()]).is_empty());
        assert!(bm25_scores("query", &[]).is_empty());
        assert!(bm25_scores("the", &["the the".to_string()]).is_empty());
    }

    #[test]
    fn relevant_doc_ranks_first() {
        let docs = vec![
            "Mobile app: offline mode and sync".to_string(),
            "Auth: OAuth login flow".to_string(),
            "Billing: invoice PDF export".to_string(),
        ];
        let scores = bm25_scores("crash login mobile app", &docs);
        assert_eq!(scores.len(), 2, "billing doc shares no term");
        assert_eq!(scores[0].0, 0, "mobile app doc must win");
        assert!(scores[0].1 > scores[1].1);
    }

    #[test]
    fn scores_are_deterministic_on_ties() {
        let docs = vec!["alpha beta".to_string(), "alpha beta".to_string()];
        let scores = bm25_scores("alpha", &docs);
        assert_eq!(scores.len(), 2);
        assert!(scores[0].0 < scores[1].0, "equal scores tie-break on index");
        assert!((scores[0].1 - scores[1].1).abs() < f64::EPSILON);
    }
}
