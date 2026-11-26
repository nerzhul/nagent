//! JSON projections from raw GitHub API payloads to the shapes
//! consumed by the LLM tools and by the PR dashboard.
//!
//! Every projection is a pure function of the upstream `Value`;
//! missing fields degrade to `null` / empty arrays so a
//! GitHub-side schema change can never panic the server.

use serde_json::{json, Value};

/// Extract `login` from a GitHub `user` object.
pub fn login_of(v: &Value) -> Value {
    v.get("user")
        .and_then(|u| u.get("login"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Extract the label objects (name + color) from a GitHub
/// `labels` array, preserving order.
pub fn label_objects(v: &Value) -> Vec<Value> {
    v.get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .map(|l| {
                    json!({
                        "name": l.get("name"),
                        "color": l.get("color"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Project one PR from `GET /repos/{owner}/{repo}/pulls` (or
/// `/pulls/{number}`) into the dashboard table row. `repo` is the
/// `"owner/repo"` full name, injected by the caller because the
/// list endpoint's rows do not carry it.
pub fn project_pr_row(repo: &str, p: &Value) -> Value {
    json!({
        "repo": repo,
        "number": p.get("number"),
        "title": p.get("title"),
        "state": p.get("state"),
        "draft": p.get("draft"),
        "merged": p.get("merged"),
        "user": login_of(p),
        "avatar_url": p.get("user").and_then(|u| u.get("avatar_url")),
        "labels": label_objects(p),
        "head_ref": p.get("head").and_then(|h| h.get("ref")),
        "head_sha": p.get("head").and_then(|h| h.get("sha")),
        "base_ref": p.get("base").and_then(|b| b.get("ref")),
        "created_at": p.get("created_at"),
        "updated_at": p.get("updated_at"),
        "merged_at": p.get("merged_at"),
        "html_url": p.get("html_url"),
    })
}

/// Project one PR detail (`GET /repos/{owner}/{repo}/pulls/{n}`)
/// — the list row plus the diff stats and the comment count,
/// which the list endpoint does not carry. `body` is passed
/// through raw; the caller (server dashboard endpoint) applies
/// the body cap.
pub fn project_pr_detail(repo: &str, p: &Value) -> Value {
    let mut row = project_pr_row(repo, p);
    if let Some(obj) = row.as_object_mut() {
        obj.insert(
            "additions".into(),
            p.get("additions").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "deletions".into(),
            p.get("deletions").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "changed_files".into(),
            p.get("changed_files").cloned().unwrap_or(Value::Null),
        );
        obj.insert(
            "comments".into(),
            p.get("comments").cloned().unwrap_or(Value::Null),
        );
        obj.insert("body".into(), p.get("body").cloned().unwrap_or(Value::Null));
    }
    row
}

/// Project one repo object (from `GET /repos/{owner}/{repo}`,
/// `GET /orgs/{org}/repos`, or `GET /user/repos`) into the small
/// summary used by the probe-repo endpoint and the repo picker.
pub fn project_repo_summary(v: &Value) -> Value {
    json!({
        "full_name": v.get("full_name"),
        "description": v.get("description"),
        "private": v.get("private"),
        "archived": v.get("archived"),
        "default_branch": v.get("default_branch"),
        "pushed_at": v.get("pushed_at"),
        "html_url": v.get("html_url"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_pr_row_maps_the_dashboard_columns() {
        let raw = json!({
            "number": 7,
            "title": "Fix the thing",
            "state": "open",
            "draft": true,
            "merged": false,
            "user": {"login": "octo", "avatar_url": "https://avatars.example/octo"},
            "labels": [{"name": "bug", "color": "d73a4a"}, {"name": "prio"}],
            "head": {"ref": "fix/thing"},
            "base": {"ref": "main"},
            "created_at": "2026-11-20T10:00:00Z",
            "updated_at": "2026-11-25T10:00:00Z",
            "merged_at": null,
            "html_url": "https://github.com/octo/hello/pull/7",
        });
        let row = project_pr_row("octo/hello", &raw);
        assert_eq!(row["repo"], "octo/hello");
        assert_eq!(row["number"], 7);
        assert_eq!(row["title"], "Fix the thing");
        assert_eq!(row["draft"], true);
        assert_eq!(row["user"], "octo");
        assert_eq!(row["avatar_url"], "https://avatars.example/octo");
        assert_eq!(row["labels"][0]["name"], "bug");
        assert_eq!(row["labels"][0]["color"], "d73a4a");
        assert_eq!(row["labels"][1]["color"], Value::Null);
        assert_eq!(row["head_ref"], "fix/thing");
        assert_eq!(row["base_ref"], "main");
        assert_eq!(row["updated_at"], "2026-11-25T10:00:00Z");
        assert_eq!(row["html_url"], "https://github.com/octo/hello/pull/7");
    }

    #[test]
    fn project_pr_row_degrades_on_missing_fields() {
        let row = project_pr_row("octo/hello", &json!({"number": 1}));
        assert_eq!(row["number"], 1);
        assert_eq!(row["title"], Value::Null);
        assert_eq!(row["labels"], json!([]));
        assert_eq!(row["user"], Value::Null);
    }

    #[test]
    fn project_pr_detail_adds_diff_stats_and_body() {
        let raw = json!({
            "number": 7,
            "title": "t",
            "additions": 12,
            "deletions": 3,
            "changed_files": 2,
            "comments": 4,
            "body": "long body",
        });
        let row = project_pr_detail("octo/hello", &raw);
        assert_eq!(row["additions"], 12);
        assert_eq!(row["deletions"], 3);
        assert_eq!(row["changed_files"], 2);
        assert_eq!(row["comments"], 4);
        assert_eq!(row["body"], "long body");
        assert_eq!(row["repo"], "octo/hello");
    }

    #[test]
    fn project_repo_summary_keeps_the_probe_fields() {
        let raw = json!({
            "full_name": "octo/hello",
            "private": false,
            "default_branch": "main",
            "html_url": "https://github.com/octo/hello",
            "description": "demo",
        });
        let s = project_repo_summary(&raw);
        assert_eq!(s["full_name"], "octo/hello");
        assert_eq!(s["private"], false);
        assert_eq!(s["default_branch"], "main");
        assert_eq!(s["html_url"], "https://github.com/octo/hello");
    }
}
