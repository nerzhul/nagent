//! Local BM25 ranking of Shortcut epics against a story's
//! title + description — the deterministic half of the automatic
//! epic classification used by `shortcut_create_story`
//! (`auto_epic: true`).
//!
//! Pure and dependency-free: the BM25 scorer lives in
//! `nagent_support::bm25` (same recipe as the tools router). No
//! network, no LLM call — the caller decides what to do with the
//! ranked candidates (auto-assign on a confident top-1, or surface
//! the candidates and let the LLM ask the user).

use serde_json::Value;

use crate::{label_names, truncate};

/// One ranked epic candidate.
#[derive(Debug, Clone)]
pub struct EpicCandidate {
    /// Shortcut epic id (positive integer).
    pub id: u64,
    /// Epic name.
    pub name: String,
    /// BM25 score against the story text (higher = better match).
    pub score: f64,
}

/// Rank the **open** epics (not archived, state != `done`) of a
/// `GET /epics` response against `query` (typically the story's
/// `name + description`). Each epic's document is
/// `name + label names + description` (description truncated at
/// `description_cap` bytes). Returns at most `top_n` candidates,
/// best first; empty when nothing scores above zero.
pub fn rank_open_epics(
    query: &str,
    epics_raw: &Value,
    description_cap: usize,
    top_n: usize,
) -> Vec<EpicCandidate> {
    let Some(arr) = epics_raw.as_array() else {
        return Vec::new();
    };
    let mut docs: Vec<String> = Vec::new();
    let mut metas: Vec<(u64, String)> = Vec::new();
    for e in arr {
        let archived = e.get("archived").and_then(|v| v.as_bool()).unwrap_or(false);
        let state = e.get("state").and_then(|v| v.as_str()).unwrap_or("");
        if archived || state == "done" {
            continue;
        }
        let Some(id) = e.get("id").and_then(|v| v.as_u64()).filter(|n| *n > 0) else {
            continue;
        };
        let name = e
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let description = e
            .get("description")
            .and_then(|v| v.as_str())
            .map(|d| truncate(d, description_cap))
            .unwrap_or_default();
        let labels = label_names(e)
            .iter()
            .filter_map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        docs.push(format!("{name} {labels} {description}"));
        metas.push((id, name));
    }
    nagent_support::bm25::bm25_scores(query, &docs)
        .into_iter()
        .take(top_n)
        .map(|(idx, score)| EpicCandidate {
            id: metas[idx].0,
            name: metas[idx].1.clone(),
            score,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn epics() -> Value {
        json!([
            {"id": 7, "name": "Mobile app", "state": "started", "archived": false,
             "description": "Everything about the iOS and Android apps, offline sync, push notifications",
             "labels": [{"name": "mobile"}]},
            {"id": 8, "name": "Auth", "state": "started", "archived": false,
             "description": "Login, SSO, OAuth, session handling"},
            {"id": 9, "name": "Legacy migration", "state": "done", "archived": false,
             "description": "Old auth system cleanup"},
            {"id": 10, "name": "Archived epic", "state": "started", "archived": true,
             "description": "login mobile"}
        ])
    }

    #[test]
    fn open_epics_only_and_best_match_first() {
        let ranked = rank_open_epics("crash at login on the mobile app", &epics(), 4096, 3);
        assert!(ranked.iter().all(|c| c.id != 9 && c.id != 10));
        assert_eq!(ranked[0].id, 7);
        assert!(ranked[0].score > 0.0);
        // Auth shares the "login" term so it ranks, but below mobile.
        let auth = ranked.iter().find(|c| c.id == 8).expect("auth ranked");
        assert!(ranked[0].score > auth.score);
    }

    #[test]
    fn no_shared_term_yields_no_candidates() {
        let ranked = rank_open_epics("zzz qqq", &epics(), 4096, 3);
        assert!(ranked.is_empty());
    }

    #[test]
    fn top_n_is_respected_and_description_is_capped() {
        let ranked = rank_open_epics("login mobile auth", &epics(), 10, 1);
        assert_eq!(ranked.len(), 1);
    }

    #[test]
    fn non_array_input_is_empty() {
        assert!(rank_open_epics("login", &json!({}), 100, 3).is_empty());
    }
}
