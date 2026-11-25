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
