# GitHub integration (read-only)

The `github-agent` cargo feature adds a per-user **GitHub**
integration and ten **read-only** chat tools over the GitHub
REST API:

| Tool | GitHub endpoint | Args |
| --- | --- | --- |
| `github_me` | `GET /user` | — |
| `github_list_repos` | `GET /user/repos` or `GET /users/{owner}/repos` | `owner?`, `type?` (all/owner/member), `sort?` (updated/pushed/full_name/created), `limit?` (default 30, cap 100) |
| `github_get_repo` | `GET /repos/{owner}/{repo}` | `owner` + `repo` (or `repo` as `"owner/repo"`) |
| `github_list_issues` | `GET /repos/{owner}/{repo}/issues` | `repo`, `state?` (open/closed/all), `labels?`, `limit?` |
| `github_get_issue` | `GET /repos/{owner}/{repo}/issues/{n}` | `repo`, `number` |
| `github_list_prs` | `GET /repos/{owner}/{repo}/pulls` | `repo`, `state?`, `limit?` |
| `github_get_pr` | `GET /repos/{owner}/{repo}/pulls/{n}` | `repo`, `number` (body included, capped) |
| `github_list_notifications` | `GET /notifications` | `unread_only?`, `limit?` |
| `github_search_issues` | `GET /search/issues` | `query` (GitHub search syntax, e.g. `author:@me is:open`), `limit?` |
| `github_read_file` | `GET /repos/{owner}/{repo}/contents/{path}` | `repo`, `path`, `ref?` — base64-decoded, capped |

The intended workflow: the user asks "review my GitHub account
and propose a plan for next week" — the LLM chains `github_me` →
`github_list_repos` → `github_search_issues` / `github_list_prs`
→ `github_read_file`, then answers with a plan.

Write operations (`github_create_issue`, `github_create_pr`,
`github_comment`) are deliberately **out of scope for v1**: they
are not registered in `AGENT_DESCRIPTORS`, so the tool loop
returns "unknown tool" if a model tries to call them.

## Setup (per user)

1. Create a personal access token at
   <https://github.com/settings/tokens>:
   - **classic PAT** with scopes `read:user` + `repo` (the
     `repo` scope is required for private repos), or
   - **fine-grained PAT** with `Account permissions → Email
     addresses: read`, `Profile: read`, and
     `Contents: read` / `Issues: read` / `Pull requests: read`
     / `Metadata: read` on the repos you want to expose.
2. In Settings → Integrations, add the **GitHub** connector, paste
   the token, and run the probe. The probe calls
   `GET {base_url}/user` with the token and stores the returned
   `login` into the `username` field, so the UI can show
   "Connected as @octocat".
3. The token is stored AES-256-GCM encrypted in the per-user
   credentials vault under the `github` service id. It is never
   logged, never echoed back on edit, and never surfaced in an
   error string or audit row (AGENTS.md §6).

Setup-only HTTP endpoint (not an LLM tool):

```
POST /api/integrations/github/probe
{"personal_access_token": "ghp_…"}
→ 200 {"ok": true, "login": "octocat", "name": "…", "avatar_url": "…", "html_url": "…", "plan": "pro"}
```

Every probe call writes one `auth_events` row of kind
`github_probe_<outcome>` (plus a `_host:<host>` companion). The
token never lands on the audit row.

## PRs dashboard tab (pure API, no LLM)

The integration form has a **Show my PRs dashboard** checkbox
(`show_dashboard` field, wire format `"true"` / `"false"`,
optional, echoed back on edit). When it is ticked (and the service
is configured), a **PRs** tab appears in the top menu of the UI.

The tab is backed by two pure-API endpoints — the PR list never
goes through the LLM; the server reads the caller's
`github.personal_access_token` from the vault and calls the
GitHub API through the shared `nagent-github` crate:

```
GET /api/integrations/github/prs?repo=&state=open&limit=&enrich=
→ 200 {"data": {"repos": ["octocat/hello", …],   // configured dashboard_repos
                "filter": null | "owner/repo",
                "state": "open",
                "total": 12, "count": 12,
                "prs": [{repo, number, title, state, draft, user, avatar_url,
                         labels: [{name, color}], head_ref, head_sha, base_ref,
                         created_at, updated_at, merged_at, html_url,
                         ci?: {state: success|failure|pending|none|unknown,
                               detail, url}}],   // ci only with ?enrich=true
                "repo_errors": [{"repo": "…", "status": 404, "error": "…"}]}}
GET /api/integrations/github/prs/{owner}/{repo}/{number}
→ 200 {"data": {"pr": {…row…, additions, deletions, changed_files,
                       comments, body (capped at max_body_bytes)}}}
```

- `state` defaults to `open` and is clamped to `open|closed|all`;
  `limit` defaults to 50 and is clamped to `[1, max_items]`. Each
  configured repo is queried with `per_page=100` (first page),
  rows are merged sorted by `updated_at` desc, then truncated.
- `?repo=` filters to one repo and must be one of the configured
  `dashboard_repos` entries (400 otherwise).
- `?refresh=true` skips the response-cache **reads** for this call
  (fresh values still write through), so the UI's Refresh button
  shows just-merged PRs as gone immediately instead of after
  `cache_ttl_secs`. Additionally, `state=open` always drops rows
  flagged `merged` — upstream never returns them for `state=open`,
  but a cached page can predate a merge.
- `?enrich=true` attaches a `ci` object to every returned row, read
  from the PR's head SHA (`GET /repos/{o}/{r}/commits/{sha}/check-runs`,
  falling back to the combined `…/status` API when the repo has no
  check runs). It costs up to one extra call per returned row
  (after the `limit` truncation), cached per `(repo, sha)` for
  `cache_ttl_secs` — hence opt-in (the UI's "CI status" checkbox).
  A failing lookup degrades that row to `{"state": "unknown"}` and
  never fails the list.
- A repo answering 404/403 (access revoked since it was added)
  lands in `repo_errors` instead of failing the whole list; a
  global 401 returns 401 with the reconnect hint; 403/429 with
  `X-RateLimit-Remaining: 0` returns 429 with the `Retry-After`
  hint.
- Every call writes one `auth_events` row of kind
  `github_api_prs_<outcome>` / `github_api_pr_<outcome>`. The token
  never lands on the audit row.
- Responses are cached in-process for `cache_ttl_secs` (default
  60 s), keyed by `(user_id, call, args)`.

### The per-user repo list (`dashboard_repos`)

The repos shown in the dashboard are a **per-user confidential
configuration**: a `dashboard_repos` text field on the `github`
service holding a JSON array of `"owner/repo"` strings (cap 50,
enforced server-side), stored AES-256-GCM encrypted in the vault
row like every other field.

The UI manages it through a **dedicated repo picker** (opened from
the GitHub integration form's "Choose repositories…" button, or
from the "Repositories" button in the PRs tab header). The picker
lists the **owners the saved PAT has access to** — the user's own
login plus every org they belong to — then shows the selected
owner's repos as a checkbox list; repos already in
`dashboard_repos` render pre-checked. It is backed by two more
pure-API endpoints (vault PAT, never the LLM):

```
GET /api/integrations/github/owners
→ 200 {"data": {"user": "octocat",
                "owners": [{"login": "octocat", "via": "self"},
                           {"login": "acme", "via": "org", "avatar_url": "…"},
                           {"login": "secretcorp", "via": "repo"}]}}

GET /api/integrations/github/owners/{owner}/repos
→ 200 {"data": {"owner": "secretcorp", "count": 1,
                "repos": [{full_name, description, private, archived,
                           default_branch, pushed_at, html_url}]}}
```

- Both endpoints draw from **one access-scoped source**:
  `GET /user/repos?visibility=all&affiliation=owner,collaborator,organization_member`
  walked page by page (`per_page=100`, up to 10 pages = 1000 repos,
  sorted by `pushed` desc; a short page ends the walk early; the
  merged list is cached for `cache_ttl_secs`). Pagination matters:
  the first page alone would silently drop repos that were not
  pushed recently. `visibility=all` is
  what surfaces **private** repos, and the `collaborator` affiliation
  is what surfaces private repos of orgs the user is only an
  **outside collaborator** on. `type` is deliberately not sent:
  GitHub answers 422 when `type` is combined with `visibility` or
  `affiliation`. The owners list is `/user/orgs` ∪ the distinct `owner.login`
  of that repo list, so an org whose membership is private (hidden
  from `GET /user/orgs` without `read:org`) still appears as long as
  one of its repos is accessible — flagged `via: "repo"`.
  `owners/{owner}/repos` is the same repo list filtered by
  `owner.login`. An owner the PAT cannot see yields an empty list.
  **Archived repos are hidden by default**; pass
  `?include_archived=true` (the picker's "Show archived" checkbox)
  to list them too — they render with an `archived` badge.
- Net effect: the picker can only ever offer repos the token can
  actually read — **checking a box is an access-verified add**. What
  the token can read depends on the PAT: a classic PAT needs the
  `repo` scope (plus `read:org` to see private org memberships as
  orgs); a fine-grained PAT needs `Contents: read` + `Pull requests:
  read` on the target repos and the org permissions on the orgs.
  If an org enforces **SAML SSO**, the token must also be authorized
  for that org ("Authorize SSO" on the token's page on GitHub) —
  otherwise that org's private repos stay invisible to the API even
  with the `repo` scope, and no query parameter can surface them.
- The owner name is validated with the same GitHub name rules as
  the chat tools before any network call (400 otherwise).
- Both calls are cached for `cache_ttl_secs`.
- Saving writes **only** `dashboard_repos` through
  `PUT /api/integrations/github/credentials` — the PUT handler
  carries the other fields' ciphertext over verbatim, so the PAT
  is never re-sent.
- Every call writes one `auth_events` row of kind
  `github_api_owners_<outcome>` / `github_api_owner_repos_<outcome>`.
  The token never lands on the audit row.

For scripted / programmatic adds, the setup-only probe below remains
available: it reads the PAT **from the vault** (unlike
`POST /api/integrations/github/probe`, which takes the token in the
request body) and checks `GET /repos/{owner}/{repo}` before a repo
is accepted. The `repo` value is validated with the same
`owner/repo` rules before any network call.

```
POST /api/integrations/github/probe-repo
{"repo": "octocat/hello-world"}
→ 200 {"ok": true, "repo": {"full_name": "…", "private": false,
                             "default_branch": "main", "html_url": "…"}}
→ 404 {"error": "no access to this repo with the saved token (or it does not exist)"}
→ 401 {"error": "GitHub rejected the personal access token (HTTP 401); …"}
```

Every probe call writes one `auth_events` row of kind
`github_probe_repo_<outcome>` (plus a `_host:<host>` companion);
the token never lands on the audit row.

## Configuration

### `[agents.github]` (operator, optional)

```toml
[agents.github]
timeout_ms     = 10_000
max_items      = 100        # hard cap on `limit` for every list tool
max_body_bytes = 65_536     # cap on issue/PR/file bodies the LLM sees
cache_ttl_secs = 60         # 0 disables the in-process cache
allowlist      = ["api.github.com"]
base_url       = "https://api.github.com"   # overridable for tests / GHES
```

Env equivalents (matching the existing `env > TOML > default`
precedence):

| TOML field                     | Env var                     |
| ---                            | ---                         |
| `agents.github.timeout_ms`     | `GITHUB_AGENT_TIMEOUT_MS`   |
| `agents.github.max_items`      | `GITHUB_AGENT_MAX_ITEMS`    |
| `agents.github.max_body_bytes` | `GITHUB_AGENT_MAX_BODY_BYTES` |
| `agents.github.cache_ttl_secs` | `GITHUB_AGENT_CACHE_TTL_SECS` |
| `agents.github.allowlist`      | `GITHUB_AGENT_ALLOWLIST`    |
| `agents.github.base_url`       | `GITHUB_AGENT_BASE_URL`     |

### GitHub Enterprise Server (GHES)

Point both knobs at your GHES host:

```toml
[agents.github]
base_url  = "https://ghes.example.com/api/v3"
allowlist = ["ghes.example.com"]
```

## Behaviour notes

- **Headers** — every request carries `Authorization: Bearer
  <PAT>`, `Accept: application/vnd.github+json`,
  `X-GitHub-Api-Version: 2022-11-28`, and a
  `User-Agent: nagent-github-agent/<version>` (GitHub rejects
  requests without a `User-Agent`).
- **Error mapping** — 401 → "reconnect via
  /settings/integrations" (PATs do not expire unless configured
  to; a 401 means the token was revoked or is wrong); 403/429
  with `X-RateLimit-Remaining: 0` → `RateLimited` with the
  `Retry-After` / `X-RateLimit-Reset` hint; 404 → upstream 404;
  any other non-2xx → upstream error with the body truncated to
  2 KiB.
- **Cache** — responses are cached in-process per
  `(user_id, tool, args)` for `cache_ttl_secs` (default 60 s).
  GitHub PAT quota is 5 000 req/h, so repeated "what's open?"
  questions stay cheap. `cache_ttl_secs = 0` disables it.
- **Body cap** — issue/PR bodies and file contents are truncated
  at `max_body_bytes` (default 64 KiB) with a `…[truncated]`
  marker before the LLM sees them.
- **SSRF posture** — every URL goes through the shared egress
  pre-flight (scheme allow-list, DNS resolution, IP policy) and
  the `allowlist`. The default allowlist is
  `["api.github.com"]`, so github.com works out of the box and
  any other host is refused until the operator widens the list
  (GHES deployments set both `base_url` and `allowlist`); an
  explicitly empty allowlist fails closed.
- **Confirmation** — every tool is read-only but still hits a
  third-party with the user's credentials, so every call fires
  the inline approval card (strict per-call; the session-wide
  "Toujours pour cette session" override is allowed).
