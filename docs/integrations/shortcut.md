# Shortcut integration (read + story creation)

The `shortcut-agent` cargo feature adds a per-user **Shortcut**
(<https://app.shortcut.com>, ex-Clubhouse) integration and
thirteen chat tools over the Shortcut REST API v3 — twelve
read-only plus one write (`shortcut_create_story`):

| Tool | Shortcut endpoint | Args |
| --- | --- | --- |
| `shortcut_me` | `GET /members` (member matched by mention name) | — |
| `shortcut_list_workflows` | `GET /workflows` | — (state id → name/category map) |
| `shortcut_list_projects` | `GET /projects` | `limit?` (default 50, cap 200) |
| `shortcut_list_epics` | `GET /epics` | `state?` (unstarted/started/done, client-side filter), `limit?` |
| `shortcut_get_epic` | `GET /epics/{id}` | `epic_id` (description capped) |
| `shortcut_list_iterations` | `GET /iterations` | `limit?` (default 20; done iterations excluded) |
| `shortcut_search_stories` | `GET /search/stories?query=…&page_size=…` | `query` (Shortcut search syntax), `limit?` (default 30) |
| `shortcut_list_backlog` | `GET /search/stories` with the configured `backlog_query` | `limit?` (default 50, cap 200) |
| `shortcut_get_story` | `GET /stories/{id}` + `GET /stories/{id}/comments` | `story_id` (description/comments capped) |
| `shortcut_list_labels` | `GET /labels` | `limit?` (default 100, cap 200) |
| `shortcut_list_teams` | `GET /teams` | — (UUID / name / mention name per team) |
| `shortcut_list_templates` | local read of the `story_templates` vault field | — (no API call, no confirmation) |
| `shortcut_create_story` | `POST /stories` | see below — **write**, confirm-on-write |

The intended read workflow: the user asks "read my Shortcut backlog
and propose a plan for the next iteration" — the LLM chains
`shortcut_me` → `shortcut_list_workflows` →
`shortcut_list_backlog` / `shortcut_search_stories` →
`shortcut_get_story` / `shortcut_get_epic` on the relevant items
→ answers with a prioritised plan.

## Creating stories from the chat

`shortcut_create_story` is the first **write** tool of the family.
The user asks e.g. *"crée une story raccourci bug : crash au login
sur mobile"* and the LLM calls it with the resolved arguments; the
inline approval card fires on every call and the session-wide
"Toujours pour cette session" override is allowed (same posture as
`caldav_create_event`).

```
args: {
  name: string (required),
  template?: string,             // a saved story shortcut (story_templates)
  story_type?: "feature"|"bug"|"chore",
  description?: string,
  details?: string,              // substituted into the template's {details}
  labels?: string[],
  epic_id?: int, project_id?: int, workflow_state_id?: int,
  team?: string,                 // UUID, name or mention name; default = saved default_team
  auto_epic?: bool (default true)
}
→ {"ok": true, "data": {"story": {…}, "template": "bug",
   "team": {"id": "…", "source": "default_team"}, "owner_id": "…",
   "epic_match": {"epic_id": 7, "auto": true}            // or "epic_candidates": […]}}
```

Resolution order (explicit args always win):

1. **`template`** — matched by name (case-insensitive) in the
   per-user `story_templates` vault field (listed by
   `shortcut_list_templates`). The template's `story_type`,
   `labels` and `description_template` become defaults;
   `{name}` / `{details}` placeholders are substituted.
2. **`story_type`** — arg, else template. When neither provides
   one, the tool fails with an `InvalidArguments` error telling the
   LLM to ask the user whether the story is a feature, a bug or a
   chore — the model never guesses.
3. **`team`** — matched against `GET /teams` by UUID / name /
   mention name, else the saved `default_team` field; the UUID is
   sent as `group_id`.
4. **`owner_ids`** — the caller's own member, resolved from the
   saved `username` against `GET /members`, so the story is
   created in their name.
5. **`epic_id`** — explicit arg, else (with `auto_epic`, default
   true) a local BM25 ranking of the open epics against the
   story's name + description (see below).

After a successful create, the cached story lists
(`api_list_stories`, `shortcut_list_backlog`,
`shortcut_search_stories`) are invalidated so the next poll
re-fetches. Writes are never cached.

### Automatic epic classification

With `auto_epic: true` (default) and no explicit `epic_id`, the
agent ranks the workspace's **open** epics (`GET /epics`, filtered
to `!archived` and `state != done`) against the story text using the
shared BM25 ranker (`nagent_support::bm25`, same recipe as the
tools router — no embeddings, no extra LLM call). The document for
each epic is `name + label names + description` (capped at
`max_body_bytes`). The top candidate is attached automatically
when its score is at least `1.0` **and** at least `1.5×` the
runner-up's score; otherwise the story is created without an epic
and the response carries `epic_candidates` (top 3 with scores) so
the LLM can tell the user which epics were close. Thresholds live in
`create_story.rs` (`AUTO_EPIC_MIN_SCORE` / `AUTO_EPIC_MIN_RATIO`).

Comment creation (`POST /stories/{id}/comments`) is wired in
`nagent-shortcut` (`add_story_comment`) but **no chat tool is
registered for it yet**; move / delete stay out of scope.

## Authentication (per user)

Two auth modes share the `shortcut` service in the vault. **OAuth is
the preferred mode** when the operator enabled `[shortcut_oauth]`
(see Configuration below); the classic API token stays as a
fallback so existing vaults keep working.

### OAuth — "Connect with Shortcut" (preferred)

This is the standard authorization-code flow: the user clicks
**Connect with Shortcut** in Settings → Integrations, is redirected
to Shortcut's authorize page
(`https://app.shortcut.com/oauth/authorize?client_id=…&redirect_uri=…&code_challenge=…`,
PKCE S256), signs in to Shortcut and approves the app, and
Shortcut redirects back to nagent's
`/api/auth/login/shortcut/callback?code=…&state=…`. nagent exchanges
the code at the token endpoint (`https://app.shortcut.com/oauth/token`),
validates the fresh bearer against `GET {api_base_url}/members`, and
stores the token pair in the vault. API calls then carry
`Authorization: Bearer <access_token>` instead of
`Shortcut-Token: <api_token>`.

The token response's non-standard `permission_id` field (the agent
app's member id in the workspace) is stored too, as is the expiry
(`token_expires_at`, RFC 3339) and the granted `token_scope`.

Wire surface (all behind `RequireAuth`, bound to the calling user
via the one-shot `state` store):

```
GET  /api/auth/login/shortcut/start       → 302 to the authorize URL
GET  /api/auth/login/shortcut/callback    → exchange code, validate, merge vault rows,
                                            302 to /settings/integrations?shortcut_connected=connected
POST /api/auth/login/shortcut/disconnect  → delete the OAuth vault rows only
                                            (the api_token fallback survives), 204
```

Audit rows: `credential_oauth_shortcut_connected` /
`credential_oauth_shortcut_disconnected` (provider `oauth_shortcut`,
target service `shortcut`). No token ever lands on an audit row.

**Refresh** — the `shortcut_*` chat tools refresh the access token
themselves: proactively when `token_expires_at` is within 60 s, and
once as a retry when the API answers 401. The refreshed
`access_token` / `refresh_token` / `token_expires_at` are written
back to the vault through the `SecretSink` (same locked pattern as
the X integration). The refresh call reads
`SHORTCUT_OAUTH_CLIENT_ID` / `SHORTCUT_OAUTH_CLIENT_SECRET` /
`SHORTCUT_OAUTH_TOKEN_URL` from the server environment. The
dashboard endpoints (pure API) do **not** refresh — a 401 there just
surfaces the reconnect hint.

### API token (fallback)

1. Generate an API token at
   <https://app.shortcut.com/settings/account/api-tokens>.
2. In Settings → Integrations, add the **Shortcut
   (app.shortcut.com)** connector, paste the token and your mention
   name, and run the probe. The probe calls `GET {base_url}/members`
   with the token and picks the member whose `mention_name` (or
   display name / email) matches the `username` field — the v3 API has
   no `/members/me` route (`/members/{member-public-id}` requires a
   UUID, so `/members/me` answers 400). The returned `mention_name`
   is stored into the `username` field, so the UI can show
   "Connected as @jane".
3. The token is stored AES-256-GCM encrypted in the per-user
   credentials vault under the `shortcut` service id. It is never
   logged, never echoed back on edit, and never surfaced in an
   error string or audit row (AGENTS.md §6).

When both are saved, the OAuth `access_token` wins everywhere
(chat tools, dashboard endpoints, probes).

### Vault fields of the `shortcut` service

| Field | Kind | Written by |
| --- | --- | --- |
| `access_token` | Password | OAuth callback + agent refresh |
| `refresh_token` | Password | OAuth callback + agent refresh |
| `token_expires_at` | Text (RFC 3339) | OAuth callback + agent refresh |
| `token_scope` | Text | OAuth callback |
| `permission_id` | Text | OAuth callback (token response) |
| `auth_mode` | Text (`oauth` / `api_token`) | OAuth callback |
| `api_token` | Password (fallback) | user, via the form |
| `username` | Text | probe (mention name) |
| `show_dashboard` | Checkbox | user |
| `default_team` | Text (JSON `{"id","name"}`) | team picker |
| `story_templates` | Text (JSON array) | user |

The OAuth callback writes its rows with a **merge** (not the
atomic full-service replace), so connecting via OAuth never erases
a saved `api_token` / `username` / `default_team`.

The six OAuth rows are `managed_fields` on the `ServiceDef`: the
integrations form does **not** render them as inputs (they are
collected by the redirect flow, not typed). The form shows the
**Connect with Shortcut** button plus a read-only status (auth mode,
token expiry) instead; because the form only submits the controls
it renders, the saved OAuth rows are never overwritten by a plain
form save.

Setup-only HTTP endpoints (not LLM tools) — both probes accept
either `{"api_token": "…"}` or `{"access_token": "…"}` (exactly one):

```
POST /api/integrations/shortcut/probe
{"api_token": "…", "username": "jane"}        // or {"access_token": "…", …}
→ 200 {"ok": true, "id": "<member-uuid>", "name": "Jane Doe", "mention_name": "jane"}

POST /api/integrations/shortcut/probe-teams
{"api_token": "…"}                              // or {"access_token": "…"}
→ 200 {"ok": true, "count": 2, "teams": [{id, name, mention_name, workflow_ids, …}]}
```

`username` is optional on the member probe: without it the probe
only succeeds when the workspace lists a single member. The
`probe-teams` endpoint never reads the vault — the body credential
drives the `GET /teams` call — so the "Default team" dropdown works
before anything is saved.

Every probe call writes one `auth_events` row of kind
`shortcut_probe_<outcome>` (plus a `_host:<host>` companion). The
token never lands on the audit row.

### Default team + story shortcuts (per-user fields)

Two optional fields on the same form feed the write path:

- **`default_team`** — stored as `{"id": "<team-uuid>", "name":
  "Engineering"}` and picked from a dropdown (never typed by hand).
  The dropdown is fed by `GET /api/integrations/shortcut/teams`
  (proxies `GET /teams` with the saved vault token) and, before the
  token is saved, by the setup-only `POST
  /api/integrations/shortcut/probe-teams` with the token typed in
  the form (same pattern as the CalDAV `probe-calendars` picker and
  the GitHub repo picker). `shortcut_create_story` sends the UUID as
  `group_id` when the chat request does not name a team. Audit
  kinds: `shortcut_api_teams_<outcome>` /
  `shortcut_probe_teams_<outcome>`.
- **`story_templates`** — a JSON array of story shortcuts, edited as
  text in the form, e.g.
  `[{"name":"bug","story_type":"bug","labels":["bug"],"description_template":"## Steps\n{details}"}]`.
  `shortcut_list_templates` lists them (local vault read, no API
  call, no confirmation) and `shortcut_create_story`'s `template`
  argument applies one.

Both fields are `echo_on_edit` (not secrets; rows still encrypted at
rest) and round-trip through `GET /api/integrations/shortcut`.

## Stories dashboard tab (pure API, no LLM)

The integration form has a **Show my stories dashboard** checkbox
(`show_dashboard` field, wire format `"true"` / `"false"`, optional,
echoed back on edit). When it is ticked (and the service is
configured), a **Stories** tab appears in the top menu of the UI.

The tab is backed by pure-API endpoints — the story list and
detail never go through the LLM; the server reads the caller's
credential from the vault (the OAuth `access_token` when present,
else `shortcut.api_token`) and proxies the Shortcut API
through the shared `nagent-shortcut` crate:

```
GET /api/integrations/shortcut/stories?query=&limit=&include_done=
→ 200 {"data": {"query": "…", "total": 1, "count": 1,
                "stories": [{id, name, story_type, labels, …}]}}
GET /api/integrations/shortcut/stories/{id}
→ 200 {"data": {"story": {…, "description": "<full markdown>",
                          "labels": [{name, color}]},
                "comments": [{id, author_id, text, …}]}}
GET /api/integrations/shortcut/me
→ 200 {"data": {"id": "<member-uuid>", "name": "Jane Doe", "mention_name": "jane"}}
GET /api/integrations/shortcut/teams
→ 200 {"data": {"count": 2, "teams": [{id, name, mention_name, workflow_ids, …}]}}
```

The list query is composed **server-side** and always scoped to the
caller: `owner:<saved mention_name>` is prepended unless `query`
already pins an `owner:`, `!is:archived` is always applied, and
`!is:done` is applied unless `include_done=true` (the "Include done"
checkbox in the dashboard header). A free-text filter like
`is:started` therefore only ever matches the user's own stories.

`/me` resolves the vault token's member via `GET /members` (matched
by the saved `username`) so the integrations form can pre-fill the
mention-name field and show "Connected as @jane" when the service is
already configured. It never returns the token. Audit kind:
`shortcut_api_me_<outcome>`.

Saving the edit form never wipes the token: `PUT
/api/integrations/shortcut/credentials` only overwrites the fields
present in the body — declared fields absent from the body (the API
token, which is never echoed to the browser) keep their existing
encrypted rows, and an empty `fields` map is a no-op.

- Without `query`, the list defaults to `owner:<mention_name>
  !is:archived !is:done` (the saved `username` field grounds the
  `owner:` operator; done stories appear only with `include_done=true`);
  when no `username` is saved, the configured `backlog_query` is the
  base. `limit` defaults to 30 and is clamped to `[1, max_items]`.
- The detail returns the **full** description (no `max_body_bytes`
  cap — that cap only applies to what the LLM sees) and all
  comments; the UI renders both as sanitized markdown.
- Every call writes one `auth_events` row of kind
  `shortcut_api_list_<outcome>` / `shortcut_api_story_<outcome>`.
  The token never lands on the audit row.
- Error mapping: missing token → 400 "not configured"; upstream
  401 → 401 with the reconnect hint; 429 → 429 with the
  `Retry-After` hint; unknown story → 404; other non-2xx → 502.
  No error body ever contains the token (AGENTS.md §6).

## Configuration

### `[shortcut_oauth]` (operator — enables "Connect with Shortcut")

Top-level section (not under `[auth.*]`): the Shortcut OAuth flow is
a per-user integration, not a nagent login backend — same posture as
`[x_oauth]`. The operator registers one Shortcut **agent app** in
the workspace settings (app.shortcut.com → Settings → Apps), adds
`<NAGENT_AUTH_PUBLIC_URL><redirect_path>` as the app's OAuth redirect
URL under Connection, and fills the section:

```toml
[shortcut_oauth]
enabled       = true            # master switch; default false
client_id     = "…"             # the agent app's client id; empty disables the flow
client_secret = ""              # optional (confidential client)
redirect_path = "/api/auth/login/shortcut/callback"
scopes        = []              # empty by default — the agent-app flow grants workspace API access
authorize_url = "https://app.shortcut.com/oauth/authorize"
token_url     = "https://app.shortcut.com/oauth/token"
api_base_url  = "https://api.app.shortcut.com/api/v3"   # used to validate the fresh token at callback time
timeout_ms    = 8_000
```

Env equivalents (`env > TOML > default`):

| TOML field                | Env var                       |
| ---                       | ---                           |
| `shortcut_oauth.enabled`  | `SHORTCUT_OAUTH_ENABLED`      |
| `shortcut_oauth.client_id`| `SHORTCUT_OAUTH_CLIENT_ID`    |
| `shortcut_oauth.client_secret` | `SHORTCUT_OAUTH_CLIENT_SECRET` |
| `shortcut_oauth.redirect_path` | `SHORTCUT_OAUTH_REDIRECT_PATH` |
| `shortcut_oauth.scopes`   | `SHORTCUT_OAUTH_SCOPES`       |
| `shortcut_oauth.authorize_url` | `SHORTCUT_OAUTH_AUTHORIZE_URL` |
| `shortcut_oauth.token_url`| `SHORTCUT_OAUTH_TOKEN_URL`    |
| `shortcut_oauth.api_base_url` | `SHORTCUT_OAUTH_API_BASE_URL` |
| `shortcut_oauth.timeout_ms` | `SHORTCUT_OAUTH_TIMEOUT_MS` |

When the section is enabled, the routes
`/api/auth/login/shortcut/{start,callback,disconnect}` mount (behind
`RequireAuth`), `GET /api/features` reports `"shortcut_oauth": true`,
and the integrations UI shows the **Connect with Shortcut** button.
`SHORTCUT_OAUTH_CLIENT_ID` / `SHORTCUT_OAUTH_CLIENT_SECRET` /
`SHORTCUT_OAUTH_TOKEN_URL` are also read by the `shortcut_*` chat
agents to refresh expired tokens in place.

### `[agents.shortcut]` (operator, optional)

```toml
[agents.shortcut]
timeout_ms     = 10_000
max_items      = 200        # hard cap on `limit` for every list tool
max_body_bytes = 65_536     # cap on story/epic descriptions + comments the LLM sees
cache_ttl_secs = 60         # 0 disables the in-process cache
allowlist      = ["api.app.shortcut.com"]
base_url       = "https://api.app.shortcut.com/api/v3"   # overridable for tests
backlog_query  = "is:story is:backlog !is:archived"      # default query of shortcut_list_backlog
```

Env equivalents (matching the existing `env > TOML > default`
precedence):

| TOML field                       | Env var                        |
| ---                              | ---                            |
| `agents.shortcut.timeout_ms`     | `SHORTCUT_AGENT_TIMEOUT_MS`    |
| `agents.shortcut.max_items`      | `SHORTCUT_AGENT_MAX_ITEMS`     |
| `agents.shortcut.max_body_bytes` | `SHORTCUT_AGENT_MAX_BODY_BYTES` |
| `agents.shortcut.cache_ttl_secs` | `SHORTCUT_AGENT_CACHE_TTL_SECS` |
| `agents.shortcut.allowlist`      | `SHORTCUT_AGENT_ALLOWLIST`     |
| `agents.shortcut.base_url`       | `SHORTCUT_AGENT_BASE_URL`      |
| `agents.shortcut.backlog_query`  | `SHORTCUT_AGENT_BACKLOG_QUERY` |

### The backlog query

`shortcut_list_backlog` runs `backlog_query` verbatim against
`GET /search/stories`. The default `is:story is:backlog
!is:archived` selects stories in an unstarted workflow state,
not scheduled in an iteration, not archived — Shortcut's own
definition of the backlog (the `is:backlog` operator). Shortcut
search operators combine with AND and support `!` / `-` negation;
the full list is documented at
<https://www.shortcut.com/help/fields-and-features/search-operators/>
(`is:story`, `is:unstarted` / `is:started` / `is:done`,
`state:"Ready for dev"`, `owner:jane`, `team:Engineering`,
`epic:growth`, `label:release`, `project:Mobile`, `estimate:3`,
`type:bug`, …). Operators override `backlog_query` per
deployment, e.g. to restrict the backlog to one team:
`is:story is:backlog team:Engineering !is:archived`.

## Behaviour notes

- **Headers** — every request carries one credential header chosen
  by the auth mode: `Authorization: Bearer <access_token>` for OAuth,
  `Shortcut-Token: <api_token>` for the classic token — plus
  `Accept: application/json`, `Content-Type: application/json`,
  and a `User-Agent: nagent-shortcut/<version>` (the wire client
  lives in the `nagent-shortcut` crate, shared with the dashboard
  endpoints).
- **Error mapping** — 401 → "reconnect via
  /settings/integrations (Connect with Shortcut when OAuth is
  enabled, else a fresh API token)"; chat tools refresh the OAuth
  token once before surfacing the 401; 429 → `RateLimited` with the
  `Retry-After` hint (the Shortcut quota is 200 req/min per
  token); 404 → upstream 404; any other non-2xx → upstream error
  with the body truncated to 2 KiB.
- **Cache** — responses are cached in-process per
  `(user_id, tool, args)` for `cache_ttl_secs` (default 60 s).
  Shortcut's quota is 200 req/min per token, so repeated "what's
  in my backlog?" questions stay cheap. `cache_ttl_secs = 0`
  disables it.
- **Body cap** — story/epic descriptions and comment texts are
  truncated at `max_body_bytes` (default 64 KiB) with a
  `…[truncated]` marker before the LLM sees them.
- **SSRF posture** — every URL goes through the shared egress
  pre-flight (scheme allow-list, DNS resolution, IP policy) and
  the `allowlist`. The default allowlist is
  `["api.app.shortcut.com"]`, so the hosted API works out of the
  box and any other host is refused until the operator widens
  the list; an explicitly empty allowlist fails closed.
- **Confirmation** — every read tool still hits a third-party with
  the user's credentials, so every read call fires the inline
  approval card (strict per-call; the session-wide "Toujours pour
  cette session" override is allowed). `shortcut_create_story` is a
  write: same card, and the session override is allowed too.
  `shortcut_list_templates` is a local vault read and needs no
  confirmation.
