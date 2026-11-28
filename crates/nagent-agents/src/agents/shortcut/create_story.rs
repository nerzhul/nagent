//! `shortcut_create_story` — create one story in the user's
//! Shortcut workspace (`POST /stories`). The first **write** tool
//! of the Shortcut family.
//!
//! Resolution order (explicit args always win over defaults):
//!
//! 1. `template` — a saved story shortcut (per-user `story_templates`
//!    vault field, listed by `shortcut_list_templates`). Its
//!    `story_type` / `labels` / `description_template` become the
//!    defaults; `{name}` / `{details}` placeholders in the
//!    description template are substituted.
//! 2. `story_type` — arg, else template, else the call fails with an
//!    `InvalidArguments` telling the LLM to ask the user whether the
//!    story is a feature, a bug or a chore (never guess).
//! 3. `team` — arg (UUID, name or mention name, matched against
//!    `GET /groups`), else the saved `default_team` vault field; the
//!    resolved UUID is sent as `group_id`.
//! 4. `project_id` — explicit arg only (optional numeric project id;
//!    no saved default).
//! 5. `owner_ids` — the caller's own member, resolved from the saved
//!    `username` against `GET /members`, so the story is created in
//!    their name.
//! 6. `epic_id` — arg, else (with `auto_epic`, default true) a local
//!    BM25 ranking of the open epics against the story's
//!    name + description; a confident top-1 is attached
//!    automatically (`epic_match` in the response), otherwise the
//!    top-3 are returned as `epic_candidates` and no epic is set.
//! 7. `workflow_state_id` — arg, else the saved `default_workflow_state`
//!    vault field, else the first `unstarted` state (by position) of the
//!    saved `default_workflow` vault field; when none is set the key is
//!    omitted and Shortcut uses the team's default initial state.
//!
//! The saved per-user defaults (`default_team`, `default_workflow` /
//! `default_workflow_state`, owner = the caller) apply silently: the
//! LLM must NOT ask the user for those fields when they are configured —
//! only `story_type` (when unclear) and an explicit `project_id` /
//! `epic_id` / `team` / `workflow_state_id` the user actually mentions
//! are worth a question.
//!
//! Confirmation: strict per-call inline approval card; the
//! session-wide "Toujours pour cette session" override is allowed
//! (trait default), matching `caldav_create_event`.

use async_trait::async_trait;
use secrecy::ExposeSecret;
use serde_json::{json, Value};

use super::client::ShortcutClient;
use super::{
    cached_get, cached_get_groups, confirm_reason_write, find_team, find_template, load_templates,
    optional_secret, parse_enum, parse_required_str, project_story,
};
use crate::agents::{Agent, AgentError, ConfirmationDecision, UserContext};

/// Story types accepted by `POST /stories`.
const STORY_TYPES: &[&str] = &["feature", "bug", "chore"];

/// Auto-epic thresholds (calibrate on the loopback fixtures):
/// the top candidate is attached automatically when its BM25 score
/// is at least `AUTO_EPIC_MIN_SCORE` AND at least
/// `AUTO_EPIC_MIN_RATIO` times the runner-up's score.
const AUTO_EPIC_MIN_SCORE: f64 = 1.0;
const AUTO_EPIC_MIN_RATIO: f64 = 1.5;

/// Cap on the number of labels sent with the story.
const MAX_LABELS: usize = 20;

/// Cache tool name for the `GET /groups` list used to resolve `team`.
const TEAMS_CACHE_TOOL: &str = "api_teams";
/// Cache tool name for the `GET /members` list used to resolve the owner.
const MEMBERS_CACHE_TOOL: &str = "shortcut_me";
/// Cache tool name for the `GET /epics` list used by `auto_epic`.
const EPICS_CACHE_TOOL: &str = "shortcut_auto_epic";
/// Cache tool name for the `GET /workflows` list used to derive the
/// default state from the saved `default_workflow`.
const WORKFLOWS_CACHE_TOOL: &str = "shortcut_default_workflow";

pub struct CreateStoryAgent {
    client: ShortcutClient,
}

impl CreateStoryAgent {
    pub fn new(client: ShortcutClient) -> Self {
        Self { client }
    }
}

/// Parse an optional positive-integer id argument.
fn parse_optional_id(args: &Value, key: &str) -> Result<Option<u64>, AgentError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().filter(|n| *n > 0).map(Some).ok_or_else(|| {
            AgentError::InvalidArguments(format!(
                "`{key}` must be a positive integer (the numeric Shortcut id)"
            ))
        }),
    }
}

/// Parse the `labels` array argument: strings only, trimmed,
/// non-empty, deduplicated, capped at [`MAX_LABELS`].
fn parse_labels(args: &Value) -> Result<Vec<String>, AgentError> {
    let Some(arr) = args.get("labels").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = Vec::new();
    for item in arr {
        let Some(s) = item.as_str() else {
            return Err(AgentError::InvalidArguments(
                "`labels` must be an array of strings".into(),
            ));
        };
        let s = s.trim();
        if s.is_empty() || out.iter().any(|e| e == s) {
            continue;
        }
        out.push(s.to_string());
        if out.len() >= MAX_LABELS {
            break;
        }
    }
    Ok(out)
}

/// Merge template labels with arg labels (dedup, cap).
fn merge_labels(template: Option<&Value>, arg_labels: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(t) = template {
        if let Some(arr) = t.get("labels").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    let s = s.trim();
                    if !s.is_empty() && !out.iter().any(|e| e == s) {
                        out.push(s.to_string());
                    }
                }
            }
        }
    }
    for s in arg_labels {
        if !out.iter().any(|e| e == &s) {
            out.push(s);
        }
        if out.len() >= MAX_LABELS {
            break;
        }
    }
    out
}

/// Substitute `{name}` / `{details}` placeholders in a template
/// description.
fn render_description_template(tpl: &str, name: &str, details: &str) -> String {
    tpl.replace("{name}", name).replace("{details}", details)
}

/// Parse the saved `default_team` vault field:
/// `{"id": "<team-uuid>", "name": "…"}` → `(id, name)`.
async fn load_default_team(ctx: &UserContext) -> Result<Option<(String, String)>, AgentError> {
    let Some(secret) = optional_secret(ctx.secret("shortcut", "default_team").await)? else {
        return Ok(None);
    };
    let text = secret.expose_secret();
    if text.trim().is_empty() {
        return Ok(None);
    }
    let parsed: Value = serde_json::from_str(text).map_err(|e| {
        AgentError::InvalidArguments(format!(
            "the saved `default_team` field is not valid JSON ({e}); fix it in \
             Settings → Integrations → Shortcut"
        ))
    })?;
    let id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let name = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match id {
        Some(id) => Ok(Some((id.to_string(), name))),
        None => Err(AgentError::InvalidArguments(
            "the saved `default_team` field must be a JSON object with a non-empty `id` \
             (the team UUID); pick a team in Settings → Integrations → Shortcut"
                .into(),
        )),
    }
}

/// Parse the saved `default_workflow_state` vault field:
/// `{"id": <state-id>, "name": "…", "category": "…"}` →
/// `(id, name, category)`. `id` must be a positive integer (the
/// numeric workflow state id sent as `workflow_state_id`).
async fn load_default_workflow_state(
    ctx: &UserContext,
) -> Result<Option<(u64, String, String)>, AgentError> {
    let Some(secret) = optional_secret(ctx.secret("shortcut", "default_workflow_state").await)?
    else {
        return Ok(None);
    };
    let text = secret.expose_secret();
    if text.trim().is_empty() {
        return Ok(None);
    }
    let parsed: Value = serde_json::from_str(text).map_err(|e| {
        AgentError::InvalidArguments(format!(
            "the saved `default_workflow_state` field is not valid JSON ({e}); fix it in \
             Settings → Integrations → Shortcut"
        ))
    })?;
    let id = parsed
        .get("id")
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            AgentError::InvalidArguments(
                "the saved `default_workflow_state` field must be a JSON object with a \
                 positive-integer `id` (the numeric workflow state id); pick a state in \
                 Settings → Integrations → Shortcut"
                    .into(),
            )
        })?;
    let name = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let category = parsed
        .get("category")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Some((id, name, category)))
}

/// Parse the saved `default_workflow` vault field:
/// `{"id": <workflow-id>, "name": "…"}` → `(id, name)`. `id` must
/// be a positive integer (the numeric workflow id from
/// `GET /workflows`).
async fn load_default_workflow(ctx: &UserContext) -> Result<Option<(u64, String)>, AgentError> {
    let Some(secret) = optional_secret(ctx.secret("shortcut", "default_workflow").await)? else {
        return Ok(None);
    };
    let text = secret.expose_secret();
    if text.trim().is_empty() {
        return Ok(None);
    }
    let parsed: Value = serde_json::from_str(text).map_err(|e| {
        AgentError::InvalidArguments(format!(
            "the saved `default_workflow` field is not valid JSON ({e}); fix it in \
             Settings → Integrations → Shortcut"
        ))
    })?;
    let id = parsed
        .get("id")
        .and_then(|v| v.as_u64())
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            AgentError::InvalidArguments(
                "the saved `default_workflow` field must be a JSON object with a \
                 positive-integer `id` (the numeric workflow id); pick a workflow in \
                 Settings → Integrations → Shortcut"
                    .into(),
            )
        })?;
    let name = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(Some((id, name)))
}

/// Derive the default workflow state of a workflow: the first
/// `unstarted` state by `position` (what the Shortcut UI treats as
/// the initial column). Best-effort — `None` when the workflow is
/// unknown or the `/workflows` call fails (creation must not be
/// blocked by it).
async fn derive_state_from_workflow(
    client: &ShortcutClient,
    ctx: &UserContext,
    workflow_id: u64,
) -> Option<(u64, String, String)> {
    let raw = cached_get(client, ctx, WORKFLOWS_CACHE_TOOL, "/workflows")
        .await
        .ok()?;
    let workflow = raw
        .as_array()?
        .iter()
        .find(|w| w.get("id").and_then(|v| v.as_u64()) == Some(workflow_id))?;
    let mut states: Vec<&Value> = workflow
        .get("states")
        .and_then(|s| s.as_array())?
        .iter()
        .filter(|s| {
            s.get("category").and_then(|v| v.as_str()) == Some("unstarted")
                || s.get("type").and_then(|v| v.as_str()) == Some("unstarted")
        })
        .collect();
    states.sort_by_key(|s| {
        s.get("position")
            .and_then(|v| v.as_u64())
            .unwrap_or(u64::MAX)
    });
    let first = states.first()?;
    let id = first.get("id").and_then(|v| v.as_u64())?;
    let name = first
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let category = first
        .get("category")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some((id, name, category))
}

/// Resolve the `group_id` for the story: explicit `team` arg
/// (matched against `GET /groups` by UUID / name / mention name),
/// else the saved `default_team`. Returns `(group_id, team_info)`.
async fn resolve_team(
    client: &ShortcutClient,
    ctx: &UserContext,
    team_arg: Option<&str>,
) -> Result<(Option<String>, Option<Value>), AgentError> {
    if let Some(q) = team_arg.map(str::trim).filter(|s| !s.is_empty()) {
        let teams_raw = cached_get_groups(client, ctx, TEAMS_CACHE_TOOL).await?;
        let team = find_team(&teams_raw, q).ok_or_else(|| {
            let names: Vec<String> = teams_raw
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            AgentError::InvalidArguments(format!(
                "no Shortcut team matches `{q}`; available teams: {}",
                if names.is_empty() {
                    "(none)".to_string()
                } else {
                    names.join(", ")
                }
            ))
        })?;
        let id = team
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::AgentFailed("matched team has no UUID `id`".into()))?
            .to_string();
        let info = json!({
            "id": team.get("id"),
            "name": team.get("name"),
            "mention_name": team.get("mention_name"),
            "source": "arg",
        });
        return Ok((Some(id), Some(info)));
    }
    if let Some((id, name)) = load_default_team(ctx).await? {
        let info = json!({ "id": id, "name": name, "source": "default_team" });
        return Ok((Some(id.clone()), Some(info)));
    }
    Ok((None, None))
}

/// Resolve the caller's member UUID from the saved `username`
/// (mention name) against `GET /members`. `None` when no username
/// is saved or no member matches — in that case `owner_ids` is
/// simply omitted and Shortcut defaults the owner to the token's
/// member.
async fn resolve_owner_id(client: &ShortcutClient, ctx: &UserContext) -> Option<String> {
    let username = match optional_secret(ctx.secret("shortcut", "username").await) {
        Ok(u) => u?,
        Err(_) => return None,
    };
    let members = cached_get(client, ctx, MEMBERS_CACHE_TOOL, "/members")
        .await
        .ok()?;
    let member = super::find_member(&members, Some(username.expose_secret()))?;
    member
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Auto-classify the epic: BM25-rank the open epics against the
/// story text. Returns `(epic_id_to_set, epic_match, epic_candidates)`.
async fn auto_classify_epic(
    client: &ShortcutClient,
    ctx: &UserContext,
    name: &str,
    description: &str,
    cfg_max_body_bytes: usize,
) -> (Option<u64>, Option<Value>, Option<Value>) {
    let epics_raw = match cached_get(client, ctx, EPICS_CACHE_TOOL, "/epics").await {
        Ok(raw) => raw,
        Err(e) => {
            // Classification is best-effort: a failed /epics call
            // must not block story creation.
            tracing::warn!(error = %e, "shortcut_create_story: epic auto-classification skipped");
            return (None, None, None);
        }
    };
    let query = format!("{name} {description}");
    let ranked = super::rank_open_epics(&query, &epics_raw, cfg_max_body_bytes, 3);
    if ranked.is_empty() {
        return (None, None, None);
    }
    let top = &ranked[0];
    let confident = top.score >= AUTO_EPIC_MIN_SCORE
        && (ranked.len() == 1 || top.score >= AUTO_EPIC_MIN_RATIO * ranked[1].score);
    let candidates: Vec<Value> = ranked
        .iter()
        .map(|c| json!({ "id": c.id, "name": c.name, "score": c.score }))
        .collect();
    if confident {
        let m = json!({
            "epic_id": top.id,
            "name": top.name,
            "score": top.score,
            "ratio": ranked.get(1).map(|r| top.score / r.score).unwrap_or(f64::INFINITY),
            "auto": true,
        });
        (Some(top.id), Some(m), None)
    } else {
        (None, None, Some(Value::Array(candidates)))
    }
}

#[async_trait]
impl Agent for CreateStoryAgent {
    fn name(&self) -> &str {
        "shortcut_create_story"
    }

    fn description(&self) -> &str {
        "Create a new story in the user's Shortcut workspace (`POST /stories`). Required: \
         `name`. Optional: `template` (name of a saved story shortcut from \
         `shortcut_list_templates`; its `story_type` / `labels` / `description_template` \
         become defaults and explicit args override them, `{name}` / `{details}` \
         placeholders are substituted), `story_type` (feature|bug|chore — when it is NOT \
         clear from the request, ASK the user first whether this is a feature, a bug or a \
         chore; never guess), `description`, `details`, `labels` (array of strings), \
         `epic_id`, `project_id` (optional numeric project id), `workflow_state_id` \
         (numeric state id; defaults to the saved `default_workflow_state`, else the first \
         `unstarted` state of the saved `default_workflow`, else omitted and Shortcut uses \
         the team's default initial state), \
         `team` (group UUID, name or mention name; defaults to the saved `default_team`), \
         `auto_epic` (default true: when \
         `epic_id` is absent, rank the open epics with a local BM25 against name + \
         description and attach the top epic when confident — the response then carries \
         `epic_match`; otherwise it carries `epic_candidates` and no epic is set). The \
         story is created with `group_id` = the resolved group and `owner_ids` = the \
         user's own member (resolved from the saved mention name), so it lands in their \
         default group and is assigned to them. IMPORTANT — do not ask the user for fields \
         that have saved defaults (`team`, `workflow_state_id`, owner): they are applied \
         automatically; only ask when the user explicitly requests something different, \
         or about `story_type` when it is unclear, or about `project_id` / `epic_id` when the \
         request genuinely needs one. Returns the created story plus the \
         template / team / workflow / epic resolution details. WRITE operation — always \
         wait for the user's approval card before this runs."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Story title. Required."
                },
                "template": {
                    "type": "string",
                    "description": "Name of a saved story shortcut (see shortcut_list_templates). Its story_type / labels / description_template become defaults."
                },
                "story_type": {
                    "type": "string",
                    "enum": ["feature", "bug", "chore"],
                    "description": "Story type. If not clear from the request, ask the user first (feature, bug or chore)."
                },
                "description": {
                    "type": "string",
                    "description": "Story description (markdown). Overrides the template's description_template."
                },
                "details": {
                    "type": "string",
                    "description": "Free text substituted into the template's `{details}` placeholder."
                },
                "labels": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Label names to attach (merged with the template's labels, max 20)."
                },
                "epic_id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Epic to attach the story to. When absent and auto_epic is true, one may be picked automatically."
                },
                "project_id": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Project to file the story under (optional; no saved default — pass the numeric project id)."
                },
                "workflow_state_id": { "type": "integer", "minimum": 1,
                    "description": "Workflow state (numeric state id from shortcut_list_workflows). Defaults to the saved `default_workflow_state`, else the first unstarted state of the saved `default_workflow`; when none is set, Shortcut uses the team's default initial state. Do not ask the user for this — the saved defaults apply automatically." },
                "team": {
                    "type": "string",
                    "description": "Team UUID, name or mention name. Defaults to the saved `default_team`."
                },
                "auto_epic": {
                    "type": "boolean",
                    "default": true,
                    "description": "When epic_id is absent, auto-attach the best-matching open epic if confident (BM25), else return epic_candidates."
                },
            },
            "required": ["name"],
            "additionalProperties": false,
        })
    }

    fn keywords(&self) -> &'static [&'static str] {
        &[
            "shortcut",
            "story",
            "create",
            "add",
            "new",
            "backlog",
            "ticket",
            "epic",
            "template",
            "raccourci",
        ]
    }

    fn requires_confirmation(&self, _ctx: &UserContext, args: &Value) -> ConfirmationDecision {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unnamed>");
        let story_type = args
            .get("story_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unspecified");
        let template = args
            .get("template")
            .and_then(|v| v.as_str())
            .map(|t| format!(" via template `{t}`"))
            .unwrap_or_default();
        confirm_reason_write(
            self.name(),
            &format!("create story `{name}` (type {story_type}{template})"),
        )
    }

    // `allow_session_override` keeps the trait default (`true`): the
    // "Toujours pour cette session" button is allowed for this tool,
    // same posture as `caldav_create_event`.

    fn untrusted_output(&self) -> bool {
        true
    }

    async fn invoke(&self, ctx: &UserContext, args: Value) -> Result<String, AgentError> {
        let name = parse_required_str(&args, "name")?;
        let details = args.get("details").and_then(|v| v.as_str()).unwrap_or("");

        // 1. Template resolution.
        let templates = load_templates(ctx).await?;
        let template = match args.get("template").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => {
                let found = find_template(&templates, t).ok_or_else(|| {
                    let names: Vec<&str> = templates
                        .iter()
                        .filter_map(|x| x.get("name").and_then(|v| v.as_str()))
                        .collect();
                    AgentError::InvalidArguments(format!(
                        "no story template named `{t}`; available: {}",
                        if names.is_empty() {
                            "(none saved)".to_string()
                        } else {
                            names.join(", ")
                        }
                    ))
                })?;
                Some(found)
            }
            _ => None,
        };

        // 2. story_type: arg > template > ask the user (never guess).
        let story_type = parse_enum(&args, "story_type", STORY_TYPES, "")?;
        let story_type = if !story_type.is_empty() {
            story_type
        } else if let Some(t) = template.and_then(|t| t.get("story_type").and_then(|v| v.as_str()))
        {
            if STORY_TYPES.contains(&t) {
                t.to_string()
            } else {
                return Err(AgentError::InvalidArguments(format!(
                    "the template's `story_type` (`{t}`) is not one of {}; fix the template in \
                     Settings → Integrations → Shortcut",
                    STORY_TYPES.join(", ")
                )));
            }
        } else {
            return Err(AgentError::InvalidArguments(
                "`story_type` is not clear from the request — ask the user whether this story \
                 is a feature, a bug or a chore, then call again with an explicit `story_type`"
                    .into(),
            ));
        };

        // 3. Description: arg > template (with placeholder substitution).
        let description = match args.get("description").and_then(|v| v.as_str()) {
            Some(d) if !d.trim().is_empty() => d.trim().to_string(),
            _ => template
                .and_then(|t| t.get("description_template").and_then(|v| v.as_str()))
                .map(|tpl| render_description_template(tpl, &name, details))
                .unwrap_or_default(),
        };

        // 4. Labels: template + args, merged.
        let labels = merge_labels(template, parse_labels(&args)?);

        // 5. Team → group_id.
        let team_arg = args.get("team").and_then(|v| v.as_str());
        let (group_id, team_info) = resolve_team(&self.client, ctx, team_arg).await?;

        // 6. Project → project_id (explicit arg only; no saved default).
        let project_id = parse_optional_id(&args, "project_id")?;

        // 7. Owner → owner_ids (the caller's own member).
        let owner_id = resolve_owner_id(&self.client, ctx).await;

        // 8. Epic: explicit arg, else BM25 auto-classification.
        let epic_id_arg = parse_optional_id(&args, "epic_id")?;
        let auto_epic = args
            .get("auto_epic")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let (auto_epic_id, epic_match, epic_candidates) = if let Some(id) = epic_id_arg {
            (Some(id), None, None)
        } else if auto_epic {
            auto_classify_epic(
                &self.client,
                ctx,
                &name,
                &description,
                self.client.cfg.max_body_bytes,
            )
            .await
        } else {
            (None, None, None)
        };
        let epic_id = epic_id_arg.or(auto_epic_id);

        // 9. Workflow state: explicit arg > saved
        //    `default_workflow_state` > first `unstarted` state of
        //    the saved `default_workflow` (by position) > omitted
        //    entirely (Shortcut then uses the team's default initial
        //    state).
        let workflow_state_arg = parse_optional_id(&args, "workflow_state_id")?;
        let default_workflow = load_default_workflow(ctx).await?;
        let (workflow_state_id, workflow_state_info) = if let Some(id) = workflow_state_arg {
            (Some(id), Some(json!({ "id": id, "source": "arg" })))
        } else if let Some((id, name, category)) = load_default_workflow_state(ctx).await? {
            (
                Some(id),
                Some(json!({
                    "id": id,
                    "name": name,
                    "category": category,
                    "source": "default_workflow_state",
                })),
            )
        } else if let Some((wf_id, _)) = &default_workflow {
            match derive_state_from_workflow(&self.client, ctx, *wf_id).await {
                Some((id, name, category)) => (
                    Some(id),
                    Some(json!({
                        "id": id,
                        "name": name,
                        "category": category,
                        "source": "default_workflow",
                    })),
                ),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        let mut body = json!({
            "name": name,
            "story_type": story_type,
        });
        if !description.is_empty() {
            body["description"] = Value::String(description);
        }
        if let Some(g) = &group_id {
            body["group_id"] = Value::String(g.clone());
        }
        if let Some(owner) = &owner_id {
            body["owner_ids"] = json!([owner]);
        }
        if let Some(e) = epic_id {
            body["epic_id"] = json!(e);
        }
        if let Some(p) = project_id {
            body["project_id"] = json!(p);
        }
        if let Some(w) = workflow_state_id {
            body["workflow_state_id"] = json!(w);
        }
        if !labels.is_empty() {
            body["labels"] = labels
                .iter()
                .map(|l| json!({ "name": l }))
                .collect::<Vec<_>>()
                .into();
        }

        // Goes through the wrapper so the OAuth token is resolved
        // (and refreshed once on 401) before the POST ships.
        let story = self.client.create_story(ctx, &body).await?;

        // 10. Writes invalidate the cached story lists so the next
        // `shortcut_list_backlog` / dashboard poll re-fetches.
        self.client
            .cache_invalidate_prefix(ctx.user_id(), "api_list_stories");
        self.client
            .cache_invalidate_prefix(ctx.user_id(), "shortcut_list_backlog");
        self.client
            .cache_invalidate_prefix(ctx.user_id(), "shortcut_search_stories");

        let mut data = json!({
            "story": project_story(&story),
            "template": template.and_then(|t| t.get("name").cloned()),
            "team": team_info,
            "owner_id": owner_id,
        });
        if let Some(w) = workflow_state_info {
            data["workflow_state"] = w;
        }
        if let Some((wf_id, wf_name)) = &default_workflow {
            data["workflow"] = json!({
                "id": wf_id,
                "name": wf_name,
                "source": "default_workflow",
            });
        }
        if let Some(m) = epic_match {
            data["epic_match"] = m;
        }
        if let Some(c) = epic_candidates {
            data["epic_candidates"] = c;
        }
        serde_json::to_string(&json!({ "ok": true, "data": data }))
            .map_err(|e| AgentError::AgentFailed(format!("shortcut encode: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_optional_id_accepts_positive_ints_only() {
        assert_eq!(parse_optional_id(&json!({}), "epic_id").unwrap(), None);
        assert_eq!(
            parse_optional_id(&json!({"epic_id": 42}), "epic_id").unwrap(),
            Some(42)
        );
        assert!(parse_optional_id(&json!({"epic_id": 0}), "epic_id").is_err());
        assert!(parse_optional_id(&json!({"epic_id": "42"}), "epic_id").is_err());
    }

    #[test]
    fn parse_labels_trims_dedups_and_caps() {
        let args = json!({"labels": ["bug", " bug ", "", 42]});
        assert!(parse_labels(&args).is_err(), "non-string item must fail");
        let args = json!({"labels": ["bug", " bug ", "", "ui"]});
        assert_eq!(parse_labels(&args).unwrap(), vec!["bug", "ui"]);
        let many = json!({"labels": (0..30).map(|i| format!("l{i}")).collect::<Vec<_>>()});
        assert_eq!(parse_labels(&many).unwrap().len(), MAX_LABELS);
    }

    #[test]
    fn merge_labels_puts_template_first_and_dedups() {
        let tpl = json!({"labels": ["bug", "ui"]});
        let merged = merge_labels(Some(&tpl), vec!["ui".to_string(), "infra".to_string()]);
        assert_eq!(merged, vec!["bug", "ui", "infra"]);
    }

    #[test]
    fn render_description_template_substitutes_placeholders() {
        let out = render_description_template("## {name}\nSteps: {details}", "Crash", "open app");
        assert_eq!(out, "## Crash\nSteps: open app");
    }

    #[test]
    fn schema_requires_name_and_locks_story_type() {
        let agent = CreateStoryAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let schema = agent.parameters_schema();
        assert_eq!(schema["required"], json!(["name"]));
        assert_eq!(
            schema["properties"]["story_type"]["enum"],
            json!(["feature", "bug", "chore"])
        );
        assert_eq!(schema["properties"]["auto_epic"]["default"], true);
    }

    struct NoSecrets;

    #[async_trait::async_trait]
    impl crate::agents::SecretSource for NoSecrets {
        async fn fetch(
            &self,
            _user_id: uuid::Uuid,
            _service: &str,
            _field: &str,
        ) -> Result<Option<secrecy::SecretString>, AgentError> {
            Ok(None)
        }
    }

    /// `SecretSource` serving one saved `default_workflow_state` row.
    struct WorkflowStateSecrets(&'static str);

    #[async_trait::async_trait]
    impl crate::agents::SecretSource for WorkflowStateSecrets {
        async fn fetch(
            &self,
            _user_id: uuid::Uuid,
            service: &str,
            field: &str,
        ) -> Result<Option<secrecy::SecretString>, AgentError> {
            if service == "shortcut" && field == "default_workflow_state" {
                Ok(Some(secrecy::SecretString::from(self.0)))
            } else {
                Ok(None)
            }
        }
    }

    fn ctx_with_secrets(secrets: std::sync::Arc<dyn crate::agents::SecretSource>) -> UserContext {
        UserContext::new(
            uuid::Uuid::new_v4(),
            std::sync::Arc::new(crate::ServiceRegistry::empty()),
            secrets,
        )
    }

    #[tokio::test]
    async fn load_default_workflow_state_parses_id_name_category() {
        let ctx = ctx_with_secrets(std::sync::Arc::new(WorkflowStateSecrets(
            r#"{"id": 500000002, "name": "Ready for Dev", "category": "unstarted"}"#,
        )));
        let (id, name, category) = load_default_workflow_state(&ctx)
            .await
            .unwrap()
            .expect("saved state present");
        assert_eq!(id, 500000002);
        assert_eq!(name, "Ready for Dev");
        assert_eq!(category, "unstarted");
    }

    #[tokio::test]
    async fn load_default_workflow_state_absent_or_blank_is_none() {
        let ctx = ctx_with_secrets(std::sync::Arc::new(NoSecrets));
        assert!(load_default_workflow_state(&ctx).await.unwrap().is_none());
        let ctx = ctx_with_secrets(std::sync::Arc::new(WorkflowStateSecrets("   ")));
        assert!(load_default_workflow_state(&ctx).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn load_default_workflow_state_rejects_bad_json_and_bad_id() {
        let ctx = ctx_with_secrets(std::sync::Arc::new(WorkflowStateSecrets("not json")));
        let err = load_default_workflow_state(&ctx).await.unwrap_err();
        assert!(matches!(err, AgentError::InvalidArguments(_)));
        let ctx = ctx_with_secrets(std::sync::Arc::new(WorkflowStateSecrets(r#"{"id": 0}"#)));
        assert!(load_default_workflow_state(&ctx).await.is_err());
        let ctx = ctx_with_secrets(std::sync::Arc::new(WorkflowStateSecrets(
            r#"{"name": "No id"}"#,
        )));
        assert!(load_default_workflow_state(&ctx).await.is_err());
    }

    /// `SecretSource` serving one saved `default_workflow` row.
    struct WorkflowSecrets(&'static str);

    #[async_trait::async_trait]
    impl crate::agents::SecretSource for WorkflowSecrets {
        async fn fetch(
            &self,
            _user_id: uuid::Uuid,
            service: &str,
            field: &str,
        ) -> Result<Option<secrecy::SecretString>, AgentError> {
            if service == "shortcut" && field == "default_workflow" {
                Ok(Some(secrecy::SecretString::from(self.0)))
            } else {
                Ok(None)
            }
        }
    }

    #[tokio::test]
    async fn load_default_workflow_parses_id_and_name() {
        let ctx = ctx_with_secrets(std::sync::Arc::new(WorkflowSecrets(
            r#"{"id": 500000002, "name": "Design"}"#,
        )));
        let (id, name) = load_default_workflow(&ctx)
            .await
            .unwrap()
            .expect("saved workflow present");
        assert_eq!(id, 500000002);
        assert_eq!(name, "Design");
    }

    #[tokio::test]
    async fn load_default_workflow_absent_blank_or_invalid_is_none_or_err() {
        let ctx = ctx_with_secrets(std::sync::Arc::new(NoSecrets));
        assert!(load_default_workflow(&ctx).await.unwrap().is_none());
        let ctx = ctx_with_secrets(std::sync::Arc::new(WorkflowSecrets("  ")));
        assert!(load_default_workflow(&ctx).await.unwrap().is_none());
        let ctx = ctx_with_secrets(std::sync::Arc::new(WorkflowSecrets("nope")));
        assert!(load_default_workflow(&ctx).await.is_err());
        let ctx = ctx_with_secrets(std::sync::Arc::new(WorkflowSecrets(r#"{"id": -3}"#)));
        assert!(load_default_workflow(&ctx).await.is_err());
    }

    #[test]
    fn confirmation_reason_names_the_story() {
        let agent = CreateStoryAgent::new(ShortcutClient::new(
            crate::ShortcutAgentConfig::default(),
            reqwest::Client::new(),
        ));
        let ctx = UserContext::new(
            uuid::Uuid::new_v4(),
            std::sync::Arc::new(crate::ServiceRegistry::empty()),
            std::sync::Arc::new(NoSecrets),
        );
        let d = agent.requires_confirmation(&ctx, &json!({"name": "Crash", "story_type": "bug"}));
        match d {
            ConfirmationDecision::NeedsConfirmation { reason } => {
                assert!(reason.contains("Crash"));
                assert!(reason.contains("bug"));
            }
            other => panic!("expected NeedsConfirmation, got {other:?}"),
        }
        // Session override stays allowed (trait default).
        assert!(agent.allow_session_override());
    }
}
