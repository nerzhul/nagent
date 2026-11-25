# Shortcut integration (read-only)

The `shortcut-agent` cargo feature adds a per-user **Shortcut**
(<https://app.shortcut.com>, ex-Clubhouse) integration and ten
**read-only** chat tools over the Shortcut REST API v3:

| Tool | Shortcut endpoint | Args |
| --- | --- | --- |
| `shortcut_me` | `GET /members/me` | — |
| `shortcut_list_workflows` | `GET /workflows` | — (state id → name/category map) |
| `shortcut_list_projects` | `GET /projects` | `limit?` (default 50, cap 200) |
| `shortcut_list_epics` | `GET /epics` | `state?` (unstarted/started/done, client-side filter), `limit?` |
| `shortcut_get_epic` | `GET /epics/{id}` | `epic_id` (description capped) |
| `shortcut_list_iterations` | `GET /iterations` | `limit?` (default 20; done iterations excluded) |
| `shortcut_search_stories` | `GET /search/stories?query=…&page_size=…` | `query` (Shortcut search syntax), `limit?` (default 30) |
| `shortcut_list_backlog` | `GET /search/stories` with the configured `backlog_query` | `limit?` (default 50, cap 200) |
| `shortcut_get_story` | `GET /stories/{id}` + `GET /stories/{id}/comments` | `story_id` (description/comments capped) |
| `shortcut_list_labels` | `GET /labels` | `limit?` (default 100, cap 200) |

The intended workflow: the user asks "read my Shortcut backlog
and propose a plan for the next iteration" — the LLM chains
`shortcut_me` → `shortcut_list_workflows` →
`shortcut_list_backlog` / `shortcut_search_stories` →
`shortcut_get_story` / `shortcut_get_epic` on the relevant items
→ answers with a prioritised plan.

Write operations (create story / comment / move story) are
deliberately **out of scope for v1**: they are not registered in
`AGENT_DESCRIPTORS`, so the tool loop returns "unknown tool" if a
model tries to call them.

## Setup (per user)

1. Generate an API token at
   <https://app.shortcut.com/settings/account/api-tokens>.
2. In Settings → Integrations, add the **Shortcut
   (app.shortcut.com)** connector, paste the token, and run the
   probe. The probe calls `GET {base_url}/members/me` with the
   token and stores the returned `mention_name` into the
   `username` field, so the UI can show "Connected as @jane".
3. The token is stored AES-256-GCM encrypted in the per-user
   credentials vault under the `shortcut` service id. It is never
   logged, never echoed back on edit, and never surfaced in an
   error string or audit row (AGENTS.md §6).

Setup-only HTTP endpoint (not an LLM tool):

```
POST /api/integrations/shortcut/probe
{"api_token": "…"}
→ 200 {"ok": true, "id": "<member-uuid>", "name": "Jane Doe", "mention_name": "jane"}
```

Every probe call writes one `auth_events` row of kind
`shortcut_probe_<outcome>` (plus a `_host:<host>` companion). The
token never lands on the audit row.

## Configuration

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

- **Headers** — every request carries `Shortcut-Token: <token>`,
  `Accept: application/json`, `Content-Type: application/json`,
  and a `User-Agent: nagent-shortcut-agent/<version>`.
- **Error mapping** — 401 → "reconnect via
  /settings/integrations"; 429 → `RateLimited` with the
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
- **Confirmation** — every tool is read-only but still hits a
  third-party with the user's credentials, so every call fires
  the inline approval card (strict per-call; the session-wide
  "Toujours pour cette session" override is allowed).
