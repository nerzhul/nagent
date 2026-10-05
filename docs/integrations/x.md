# X (Twitter) timeline integration

`x_timeline` is a read-only chat agent that fetches the calling
user's authenticated X (Twitter) home timeline (mode "Abonnements"
/ "Following" by default, or "Pour Vous" / "For You" on demand)
and surfaces the recent posts as a JSON envelope the LLM can
cluster + summarise.

The agent reuses the existing per-user credentials vault
(`nagent_db::Credentials`) to store the user's OAuth tokens under
the `x_account` service id, and uses the existing chat-agent
wiring (`AgentRegistry` / `AGENT_DESCRIPTORS`, `EgressPool`,
`tools[]` SSE events) so it slots into the LLM tool loop without
changes to the proxy or the frontend.

## Configuration

### Top-level `[x_oauth]` (mandatory)

```toml
[x_oauth]
enabled        = true                # master switch; default false
client_id      = "..."               # from developer.x.com
client_secret  = ""                  # empty for PKCE-only public clients
redirect_path  = "/api/auth/login/x/callback"
scopes         = ["tweet.read", "users.read", "follows.read"]
timeout_ms     = 8000
```

Env equivalents (matching the existing `env > TOML > default`
precedence):

| TOML field            | Env var                 |
| ---                   | ---                     |
| `x_oauth.enabled`     | `X_OAUTH_ENABLED`       |
| `x_oauth.client_id`   | `X_OAUTH_CLIENT_ID`     |
| `x_oauth.client_secret` | `X_OAUTH_CLIENT_SECRET` |
| `x_oauth.redirect_path` | `X_OAUTH_REDIRECT_PATH` |
| `x_oauth.scopes`      | `X_OAUTH_SCOPES`        |
| `x_oauth.timeout_ms`  | `X_OAUTH_TIMEOUT_MS`    |

### Per-agent `[agents.x_timeline]` (optional)

```toml
[agents.x_timeline]
timeout_ms     = 8000
max_posts      = 20                # LLM-callable upper bound (also enforced server-side)
allowlist      = ["api.x.com", "x.com"]
cache_ttl_secs = 60                # 0 disables the in-process cache
base_url       = "https://api.x.com"
```

| TOML field                  | Env var                       |
| ---                         | ---                           |
| `agents.x_timeline.timeout_ms`   | `X_TIMELINE_TIMEOUT_MS`   |
| `agents.x_timeline.max_posts`    | `X_TIMELINE_MAX_POSTS`    |
| `agents.x_timeline.allowlist`    | `X_TIMELINE_ALLOWLIST`    |
| `agents.x_timeline.cache_ttl_secs` | `X_TIMELINE_CACHE_TTL_SECS` |
| `agents.x_timeline.base_url`     | `X_TIMELINE_BASE_URL`     |

## Build / runtime gating

The X OAuth flow + the `x_timeline` agent are gated by the
`x-agent` cargo feature. Operator opt-in:

```bash
cargo build -p nagent-server --features x-agent
cargo build -p nagent-server --features all-agents   # includes x-agent
```

Every `make run*` target uses `--features all-agents`, so a
default build includes the X agent. The runtime master switches
are `x_oauth.enabled` (default `false`) and
`x_oauth.client_id` (empty = X OAuth disabled).

## Registering the X Developer app

1. Sign in at <https://developer.x.com/> and create a new
   project + app.
2. **App type**: pick "Web App, Single-Page App, or Native App"
   for PKCE-only flow (no `client_secret`). The
   "Web App, Confidential Client" type requires a
   `client_secret` and an entry in the
   `client_id` / `client_secret` pair at boot.
3. **OAuth 2.0 settings**:
   - Type of App: **Web App, SPA, or Native**.
   - Callback URL: the full URL the browser is redirected to
     after X authorisation. The default is
     `{public_url}/api/auth/login/x/callback` (set
     `X_OAUTH_REDIRECT_PATH` to override).
   - Scopes: `tweet.read`, `users.read`, `follows.read`
     (the v1 read-only surface).
4. **Client ID**: copy into `X_OAUTH_CLIENT_ID` (or
   `[x_oauth].client_id`).
5. **Client secret** (confidential-client mode only): copy into
   `X_OAUTH_CLIENT_SECRET`.

## End-user flow

1. Open `/settings/integrations` (or the chat UI's "Connect X"
   affordance).
2. Click "Connect X" → `GET /api/auth/login/x/start` →
   302 to `https://x.com/i/oauth2/authorize?…`.
3. The browser completes the X login / authorisation.
4. X redirects back to `/api/auth/login/x/callback?code=…&state=…`.
5. The callback POSTs the `code` to `/2/oauth2/token`, fetches
   `/2/users/me`, and UPSERTs the six `x_account` field rows
   under the authenticated user.
6. The user is redirected to `/settings/integrations?x_connected=1`.

To disconnect, the user clicks "Disconnect X" in the
integrations page (calls `POST /api/auth/login/x/disconnect`).
The disconnect handler deletes every `x_account` field row
for the calling user.

## Tool surface (LLM-facing)

`x_timeline` is a single read-only agent. Parameters:

```json
{
  "mode": "following" | "for_you",   // default "following"
  "max_posts": 1..=100,             // default 20
  "since_hours": 1..=168,           // optional server-side time window
  "language": "fr" | "en"           // optional soft preference
}
```

The result JSON envelope:

```json
{
  "mode": "following",
  "fetched_at": "2026-10-05T15:00:00Z",
  "count": 17,
  "posts": [
    {
      "id": "1234567890",
      "author": "@naval",
      "author_name": "Naval",
      "created_at": "2026-10-05T14:42:00Z",
      "lang": "en",
      "text": "…",
      "urls":    ["https://example.com/article"],
      "hashtags": ["ai"],
      "mentions": ["@paulg"],
      "metrics": { "likes": 412, "retweets": 88, "replies": 31, "quotes": 5 }
    }
  ],
  "shared_links": ["https://example.com/article"],
  "hint": "Posts sorted newest-first. Cluster posts that share urls/hashtags/keywords for the theme dedup."
}
```

The LLM does the theme clustering + summary in its final reply
(one tool round, no extra LLM calls).

## Token refresh

Refresh is handled by the agent itself (locked decision in plan
1790695073418). When X returns 401, the agent POSTs to
`/2/oauth2/token` with `grant_type=refresh_token` and writes
the new `access_token` + `refresh_token` + `token_expires_at`
back to the vault through
`UserContext::update_secret(...)`. The display-only fields
(`x_user_id`, `x_screen_name`, `token_scope`) survive every
refresh round-trip.

If refresh is rejected (revoked / expired refresh token), the
agent surfaces a clear reconnect hint to the LLM:
> "X rejected the access token even after refresh; reconnect X
> via /settings/integrations"

## Caching

A short in-process `Mutex<HashMap<(user_id, mode), (timestamp,
payload)>>` with `cache_ttl_secs` default 60, max 600,
prevents hammering `api.x.com` when the user asks
"résume ma timeline X" twice in a row. Keyed by `(user_id,
mode)` so different users or modes do not collide. `0` disables
the cache. The cache is per-process (no cross-instance sharing)
and never persisted to disk; if the process restarts the cache
is cold.

## Failure modes

| Condition                          | Surfaced as                                                       |
| ---                                | ---                                                               |
| User has not connected X           | `AgentError::CredentialsMissing { service: "x_account", field: "access_token" }` — chat UI prompts to connect. |
| Access token expired + refresh failed | `AgentError::AgentFailed("X OAuth refresh failed: …")`        |
| X returns 401 even after refresh   | Same as above; never loop forever.                                |
| X returns 429 (rate limit)         | `AgentError::Upstream { status: 429, body: <truncated> }`       |
| X returns 5xx                      | `AgentError::Upstream { status, body }`                          |
| Network / TLS error                | `AgentError::AgentFailed("…connect/read failed: {err}")`         |
| Malformed X JSON                   | `AgentError::AgentFailed("X response parse: {err}")`             |
| OAuth `state` mismatch / expired   | Redirect with `?x_error=expired_state`; never reveals the verifier |
| OAuth token exchange 4xx           | Redirect with `?x_error=token_exchange_failed`; logs the body at WARN, never the access_token |

## Out of scope (deliberately)

- Posting tweets (no `tweet.write` scope).
- Searching tweets by query / hashtag.
- Reading individual tweet replies / thread continuations.
- Multi-account-per-nagent-user (a second X account would
  clobber the first; explicit decision to keep
  1-account-per-nagent-user).
- Sharing the timeline across multiple nagent users (each user
  has their own OAuth tokens, no shared bot account).
- Persisting the agent's response cache to disk across
  restarts (in-process only).
- X API v1.1 endpoints.

## Rate limits

X's free / Basic tier is tight (~1500 reads/user/month). The
in-process cache + `max_posts=20` default keeps us well under,
but operators on a budget can lower `max_posts` to 5 or set
`cache_ttl_secs` to a higher value. Upgrade to the Pro tier
($5000/month) for higher quotas.

## Risks

- **X API policy drift**: X has changed scopes, endpoints, and
  pricing repeatedly. The plan pins to v2 endpoints documented
  as of 2026-10. A future policy change may require
  re-registering the app or adding the `media.fields=…`
  expansion back. Mitigation: the URL builder + scope list live
  in one file (`crates/nagent-agents/src/agents/x_timeline/mod.rs`)
  so a future change is a single-file edit.
- **PKCE-only flow without `client_secret`**: X allows PKCE
  without `client_secret` only for apps registered as "Native"
  or "Single Page App" on `developer.x.com`. The
  `redirect_path` README line above spells this out so an
  operator registering a "Web App, Confidential Client" type
  does not silently break the flow.
- **Token replay across processes**: If two server instances
  both attempt to refresh an expired token at the same
  instant, X issues two new refresh tokens; the loser's write
  is discarded. Mitigation: documented single-writer
  assumption; cluster deployments should run the OAuth route
  through a single writer.
