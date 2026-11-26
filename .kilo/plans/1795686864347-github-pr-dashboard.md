# GitHub open-PRs dashboard tab (repo-filtered, per-user confidential repo list)

Status: **implemented** in the working tree (plan written against
build `2604c1ef5cc09293`, `crates/nagent-server/src/static/version.txt`).
All phases landed: `nagent-github` crate extraction, the two
vault fields, `probe-repo`, the `prs` dashboard endpoints, the PRs
tab + repo manager UI, docs sync, and unit + e2e tests.

**Addendum — repo-management UI rework (phase 6, revised).** The
first cut managed `dashboard_repos` as chips + a free-text
`owner/repo` add button inside the integration form. Per user
feedback it was replaced by a **dedicated repo picker**
(`static/github-repo-picker.js`, exposed as
`window.__nagentGithubRepoPicker`): it lists the owners the saved
PAT can access (own login + orgs, via the new
`GET /api/integrations/github/owners` and
`GET /api/integrations/github/owners/{owner}/repos` endpoints),
then renders each owner's repos as a checkbox list with
already-subscribed repos pre-checked. Because both listing
endpoints are access-scoped upstream, ticking a box is an
access-verified add; `probe-repo` stays as the programmatic guard.
The picker opens from the integration form ("Choose
repositories…") and from the PRs tab header ("Repositories"), and
saves only the `dashboard_repos` field through the existing PUT
(the other fields' ciphertext is carried over, PAT never re-sent).

**Addendum 2 — private repos of other orgs.** The first picker
version sourced "your repos" from `/user/repos?affiliation=owner`
(owned repos only) and orgs from `/orgs/{org}/repos`, which misses
outside-collaborator private repos and orgs with a private
membership. Both `owners` endpoints now draw from the single
access-scoped list
`GET /user/repos?visibility=all&affiliation=owner,collaborator,organization_member`
(filtered by `owner.login` for the per-owner view); the owners list
is `/user/orgs` ∪ the repo owners (`via: "org"` vs `via: "repo"`),
so private repos of other orgs show up under their own owner.
`type` is deliberately not sent: GitHub answers **422** when `type`
is combined with `visibility` or `affiliation` (addendum 3).
Also fixed in passing: the picker's `apiFetch` no longer
`JSON.parse`s a 204 empty body (that was misreported as
"Save failed" even though the PUT had succeeded).

**Addendum 3 — 422 on `/user/repos`.** Sending `type=all` together
with `affiliation` is rejected by GitHub ("If you specify visibility
or affiliation, you cannot specify type"). The query now uses
`visibility=all` (which is also the default, and is compatible with
`affiliation`) and drops `type` entirely; the e2e fixture asserts
`type=` is absent from the upstream request line.

**Addendum 4 — pagination of the accessible-repo list.** The first
implementation of the single-source `/user/repos` walk read only the
first page (100 repos, `pushed` desc), which silently dropped repos
that were not pushed recently (e.g. an org's older/private repos).
`accessible_repos` now walks `page=1..=10` (`per_page=100`, up to
1000 repos) until a short page, and caches the merged raw list under
`(user_id, "api_user_repos", {})` so the owners endpoint and every
per-owner view share one upstream fetch per `cache_ttl_secs`. The
paged e2e fixture (100 repos on page 1, 2 on page 2) asserts the
walk reaches page 2, stops before page 3, and that the per-owner
view issues no extra `/user/repos` call. Reminder: if a work org
enforces SAML SSO, the PAT must be SSO-authorized for that org —
otherwise its private repos stay invisible to the API regardless of
the query (documented in `docs/integrations/github.md`).

**Addendum 5 — archived repos hidden unless asked.** `list_owner_repos`
now drops `archived: true` repos unless `include_archived` is set; the
endpoint accepts `?include_archived=true` (default false), the cache
key includes the flag, and the picker gained a "Show archived"
checkbox that refetches the current owner with the parameter. Archived
repos keep their `archived` badge when shown.

**Addendum 6 — PRs view rendered below the main UI.** The tab button
and mode wiring were correct, but `style.css` only had the
ID-specific `#view-…[hidden] { display: none; }` rule for
transcript/discussion/settings/stories — the global `main { display:
block }` author rule beats the UA `[hidden]` rule, so `#view-prs`
stayed visible under the active view. Added `#view-prs[hidden]` to
that rule; the PRs dashboard now behaves exactly like the Stories tab
(top tab, shown only when the GitHub integration's `show_dashboard`
is checked).

**Addendum 9 — refresh bypasses the cache; merged PRs hidden.**
`GET …/prs` gained `?refresh=true` (the UI Refresh button sets it):
the per-repo PR lists and per-SHA CI statuses then skip their cache
reads but still write through, so a PR merged seconds ago
disappears on Refresh instead of lingering up to `cache_ttl_secs`.
Independently, `state=open` now drops rows flagged `merged` after
the merge step (defense against a stale cached page). `list_prs_for_repo`
and `list_dashboard_prs` take parameter structs (`RepoPrsQuery` /
`DashboardPrsQuery`) to stay under clippy's argument limit. The e2e
fixture serves a merged PR (#5) and a toggleable extra PR (#11) to
assert: merged filtered from the open list, plain call cached,
`?refresh=true` sees #11, next plain call sees #11 (write-through).

**Addendum 8 — CI status + nicer PR link.** `?enrich=true` on
`GET /api/integrations/github/prs` attaches a per-row `ci` object
(`state` success/failure/pending/none/unknown, `detail`, `url` →
the PR's `/checks` tab), read from the head SHA via
`/commits/{sha}/check-runs` with a combined-`/status` fallback,
cached per `(user, repo, sha)`; enrichment happens after the
`limit` truncation so it costs at most `limit` extra calls, and a
failing lookup degrades only that row. The UI adds a "CI status"
checkbox (opt-in because of the extra calls) and a CI badge column;
the PR `#` link is now a mono pill (`.prs-link`) instead of a raw
blue link.

**Addendum 7 — loaders.** New reusable `.nagent-loader` component
(spinner ring reusing the chat status pill's `status-spin` keyframes,
`currentColor`). Shown as a full-width row inside the PRs table while
`GET …/prs` is in flight (`aria-busy` on the table, error rendered as
a row on failure), in the picker's owners pane while `GET …/owners`
loads, and in the repos pane while `GET …/owners/{owner}/repos`
loads. The `prs-status` / `gh-picker-status` lines also get a small
inline spinner whenever `data-state="pending"` (covers the picker's
save too).

## Goal

Add a **PRs dashboard** to the web UI: a clean table of the **open pull
requests** of a per-user list of GitHub repositories.

- The repository list is a **per-user confidential configuration**: it lives
  in the AES-256-GCM-encrypted per-user vault row of the `github` service
  (new `dashboard_repos` field), never in plaintext, never in a log.
- Adding a repo **verifies access with the saved GitHub credentials** before
  the repo is accepted (new setup-only probe endpoint `POST
  /api/integrations/github/probe-repo` calling `GET /repos/{owner}/{repo}`
  with the vault PAT).
- The dashboard itself is **pure API, no LLM**: `GET
  /api/integrations/github/prs` aggregates open PRs across the configured
  repos (optionally filtered to one repo with `?repo=owner/repo`) and
  projects them into table rows, mirroring the Shortcut "Stories" dashboard
  (`crates/nagent-server/src/credentials/shortcut_api.rs`).
- The tab appears in the top menu only when the integration's
  **Show my PRs dashboard** checkbox (`show_dashboard`) is ticked — same
  pattern as Shortcut's `show_dashboard` field.

Non-goals for v1: write operations (already out of scope for the whole
`github-agent` feature), PR detail pane inside the UI (the row links out to
`html_url`; a detail endpoint is specced below for v2), pagination beyond
the first page per repo, LLM tool changes (the ten `github_*` chat tools
stay untouched).

## Design decisions

### D1 — Where the repo list lives: a new vault field, not a new table

`crates/nagent-agents/src/github_service.rs` (`GITHUB_SERVICE`) gains two
fields:

| key | kind | required | echo_on_edit | meaning |
| --- | --- | --- | --- | --- |
| `show_dashboard` | Checkbox (`"true"`/`"false"`) | no | true | gates the PRs tab visibility |
| `dashboard_repos` | Text | no | true | JSON array of `"owner/repo"` strings, cap 50 entries |

Rationale: the per-user credentials vault row is already encrypted at rest
(AES-256-GCM), already scoped per user, and already round-trips through the
existing atomic `PUT /api/integrations/github/credentials` flow — so the
"confidential user config" requirement costs **no new migration** and no new
storage path. The alternative (`user_preferences` table, migration `0006+`)
stores UI prefs in plaintext and is the wrong home for a per-user list the
user considers sensitive. `dashboard_repos` is not a credential, so
`echo_on_edit = true` is fine (repo names are not secrets); the row is still
encrypted at rest.

### D2 — Access check on add: `POST /api/integrations/github/probe-repo`

Unlike the existing `POST /api/integrations/github/probe` (which takes the
token in the request body because nothing is saved yet), the repo probe runs
**after** the token is saved, so it reads the PAT **from the vault** and
calls `GET /repos/{owner}/{repo}`:

```
POST /api/integrations/github/probe-repo
{"repo": "octocat/hello-world"}
→ 200 {"ok": true, "repo": {"full_name": "octocat/hello-world", "private": false,
                             "default_branch": "main", "html_url": "…"}}
→ 404 {"error": "no access to this repo with the saved token (or it does not exist)"}
→ 401 {"error": "token rejected — reconnect GitHub in Settings → Integrations"}
```

- The `repo` argument is validated **before any network call** with a strict
  format check: `^[A-Za-z0-9](?:[A-Za-z0-9_.-]{0,38})/[A-Za-z0-9_.-]{1,100}$`
  (GitHub login/repo charset), so it can never smuggle a path traversal into
  the API path.
- Every call writes one `auth_events` row of kind `github_probe_repo_<outcome>`
  (plus the `_host:<host>` companion) via the shared `crate::probe` plumbing.
  **The token never lands on the audit row, a log line, or an error body**
  (AGENTS.md §6) — only upstream status + truncated reply body are surfaced.
- SSRF posture identical to the chat agents: egress pre-flight +
  `[agents.github].allowlist` (default `["api.github.com"]`), fail-closed.
- The UI only appends the repo to `dashboard_repos` on a 200; on 404/401 it
  shows the error and does not save.

### D3 — Dashboard endpoints are pure API (no LLM), mirroring `shortcut_api.rs`

New module `crates/nagent-server/src/credentials/github_api.rs` with a
`GithubApiState` (store + credentials key + shared client, built once at
router construction so the reqwest pool stays warm), mounted in
`http::build_router` next to `shortcut_api_routes`, feature-gated on
`github-agent` exactly like `github_probe_routes`.

```
GET /api/integrations/github/prs?repo=&state=open&limit=
→ 200 {"data": {"repos": ["octocat/hello-world", …],      // configured list
                "filter": null | "owner/repo",
                "total": 12, "count": 12,
                "prs": [ {see D4 row projection} ],
                "repo_errors": [{"repo": "…", "error": "no access"}]}}

GET /api/integrations/github/prs/{owner}/{repo}/{number}   (v2 detail pane)
→ 200 {"data": {"pr": {…, additions, deletions, changed_files, comments, body (capped)}}}
```

- `state` defaults to `open` and is clamped to `open|closed|all` (the
  dashboard is "open PRs" by default; the closed/all states are one query
  param away).
- `repo` optional filter: must be one of the configured repos, else 400.
- `limit` defaults to 50, clamped to `[1, max_items]`; each configured repo
  is queried with `per_page = 100` (first page only in v1), results are
  merged, sorted by `updated_at` desc, then truncated to `limit`.
- A repo that answers 404/403 (access revoked since it was added) does **not**
  fail the whole dashboard: it lands in `repo_errors` and the rest of the
  rows still render. A global 401 (token revoked) returns 401 with the
  reconnect hint; 403/429 with `X-RateLimit-Remaining: 0` returns 429 with
  the `Retry-After` hint — same error mapping as the chat agents.
- Every call writes one `auth_events` row of kind `github_api_prs_<outcome>`
  (distinct from `github_probe_repo_*` so operators can tell setup probes
  from dashboard reads). Token never on the row.
- Both endpoints are GET (read-only) behind `RequireAuth`, like every
  `/api/integrations*` route.
- Response cache: reuses `[agents.github].cache_ttl_secs` (default 60 s)
  keyed by `(user_id, repo, state, page)` so dashboard refreshes stay cheap
  against the 5 000 req/h PAT quota.

### D4 — Table row projection (the "metadata nécessaires de compréhension")

One row per PR, projected from `GET /repos/{owner}/{repo}/pulls`:

| column | source |
| --- | --- |
| repo | `repo_full_name` (added by the aggregator, since rows merge several repos) |
| # | `number` (rendered as a link to `html_url`) |
| title | `title` |
| author | `user.login` (+ `user.avatar_url`) |
| state / draft | `state`, `draft` badge |
| labels | `labels[].name` + `labels[].color` chips |
| branch | `head.ref` → `base.ref` |
| created | `created_at` (ISO + relative in `title`) |
| updated | `updated_at` (sort key, ISO + relative) |
| url | `html_url` |

The UI renders this as a real `<table>` (sortable client-side by # / updated),
with a repo filter `<select>` populated from `data.repos`.

### D5 — Wire client: extract a pure `nagent-github` crate (mirrors `nagent-shortcut`)

`docs/architecture.md` already blesses this pattern for Shortcut:
`nagent-shortcut` is a pure REST client (token as a parameter, response
cache, JSON projections) shared by `nagent-agents` (chat tools) and
`nagent-server` (dashboard endpoints). Today the GitHub client
(`crates/nagent-agents/src/agents/github/client.rs`) is private to
`nagent-agents` and pulls its token from `UserContext`, so the server cannot
reuse it.

Plan: create `crates/nagent-github` (`client.rs` + `project.rs` + `lib.rs`,
same layout as `crates/nagent-shortcut/`) holding a token-as-parameter
`GithubClient` (headers, egress pre-flight, allowlist, cache, error mapping)
plus the PR/issue/repo JSON projections; refactor the ten `github_*` agents
to consume it (their public behaviour and tests stay identical); add the
crate to `[workspace.members]`, to `nagent-agents` and `nagent-server`
dependencies, and forward a `github-agent` feature exactly like
`shortcut-agent` does (`nagent-server/Cargo.toml`,
`nagent-agents/Cargo.toml`, Dockerfile build args if a new feature is
introduced).

## Plan

1. **Crate extraction (`nagent-github`)** — move the wire client +
   projections out of `nagent-agents/src/agents/github/client.rs` into
   `crates/nagent-github`, with `GithubClient::new(config, http)` and
   `get(token, path, query)` taking the token as a parameter; re-export from
   `nagent-agents` so the ten agents keep compiling with a thin
   `UserContext`-based adapter (`token(ctx)` stays on the agent side).
   Update `Cargo.toml` workspace members, both dependent crates' manifests,
   and `docs/architecture.md` §"Workspace layout" (eight crates) in the same
   commit (AGENTS.md §0/§5).
2. **Service fields** — add `show_dashboard` (Checkbox) + `dashboard_repos`
   (Text, JSON array, cap 50) to `GITHUB_SERVICE` in
   `crates/nagent-agents/src/github_service.rs`, with help text explaining the
   add-repo flow. Unit tests in `services.rs` for the new field summary
   shape (mirroring the existing `show_dashboard` tests at
   `services.rs:563-584`).
3. **Probe-repo endpoint** — new
   `crates/nagent-server/src/credentials/github_probe_repo.rs`
   (`POST /api/integrations/github/probe-repo`), mounted feature-gated in
   `http/mod.rs` next to `github_probe_routes`; vault-token read + decrypt
   via the same helpers as `shortcut_api.rs::vault_token`; audit rows
   `github_probe_repo_<outcome>`; `owner/repo` regex validation before
   network I/O.
4. **Dashboard endpoints** — new
   `crates/nagent-server/src/credentials/github_api.rs`
   (`build_github_api_router`): `GET /api/integrations/github/prs` +
   `GET /api/integrations/github/prs/{owner}/{repo}/{number}`, aggregator +
   D4 projection, per-repo error isolation, cache, audit kinds
   `github_api_prs_<outcome>`; mounted in `http/mod.rs` next to
   `shortcut_api_routes`.
5. **UI tab** — `static/index.html`: `mode-prs-btn` tab + `view-prs` main
   view (table skeleton, repo filter select, refresh button, status line),
   hidden by default; `static/mode.js`: add `prs` to the mode map;
   new `static/github-prs.js` (mirrors `shortcut-stories.js`: reads
   `GET /api/integrations/github` → `show_dashboard` to show/hide the tab,
   fetches `/api/integrations/github/prs`, renders the table, repo filter,
   refresh, row → `html_url`); CSS in `static/style.css`; script tag in
   `index.html`.
6. **Integrations form repo manager** — `static/integrations.js`: for the
   `github` service, render `dashboard_repos` as a chip list with an
   `owner/repo` input + **Add** button that calls `POST
   /api/integrations/github/probe-repo` first and only appends on 200
   (404/401 shown inline, nothing saved), plus a remove button per chip;
   the JSON array is written back into the `dashboard_repos` field and saved
   through the existing `PUT /api/integrations/github/credentials` flow.
7. **Docs sync (same commit, AGENTS.md §5)** — `docs/integrations/github.md`
   gains a "PRs dashboard tab" section (endpoints, fields, probe-repo,
   error mapping, audit kinds); `docs/architecture.md` §2.4 github row and
   §2.6 mention the dashboard endpoints + `nagent-github` crate; README
   "Configuration" table unchanged (no new operator knob unless
   `dashboard_max_repos` is added — default 50 in code).
8. **Tests** — unit: `nagent-github` projection/parsing tests (PR list row,
   repo arg regex, error mapping); integration in
   `crates/nagent-server/tests/github_dashboard_e2e.rs` mirroring
   `shortcut_e2e.rs` with a loopback GHES fixture via
   `[agents.github].base_url` override: probe-repo 200/404/401 + audit rows
   without token, `prs` aggregation across two repos sorted by `updated_at`,
   `?repo=` filter + 400 on unconfigured repo, per-repo 404 isolation, token
   never present in any error body, `show_dashboard`/`dashboard_repos`
   round-trip through `PUT /api/integrations/github/credentials`.

## Hardening checklist (AGENTS.md §6)

- Token appears only in the `Authorization` header; `redact_authorization`
  (already in the github client) is used on every debug/error path.
- No audit row, log line, UI status line, or HTTP error body ever contains
  the PAT — probe-repo and dashboard errors surface only upstream status +
  truncated body.
- `dashboard_repos` is stored encrypted in the vault; the integrations UI
  never re-displays the PAT (`echo_on_edit` stays `false` on
  `personal_access_token`).
- All GitHub URLs go through the egress SSRF pre-flight + allowlist; an
  explicitly empty allowlist fails closed.
- Test fixtures assert against `<redacted>` markers, not real tokens.

## Open questions

- v1 keeps the list to the first page per repo (`per_page=100`); GitHub
  `Link`-header pagination is a v2 item if repos with >100 open PRs matter.
- Comments count / review decision are not in the `/pulls` list payload;
  they cost one extra call per PR. v1 omits them (the detail endpoint
  exposes them); a `?enrich=true` flag can add them later.
