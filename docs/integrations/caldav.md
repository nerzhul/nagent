# CalDAV integration

The CalDAV plugin (plan 1790963194218) lets each user connect
their personal calendar (Nextcloud, Radicale, Fastmail,
iCloud, …) to the chat agent. v1 ships **read + add only**:
the agent can list events, fetch a single event, and create
a new one. Edit and delete are explicitly out of scope (see
"Forbidden operations" below).

## Wire surface

| Tool | Kind | Confirmation | Args |
| --- | --- | --- | --- |
| `caldav_list_events` | read | allow | `start`, `end`, `calendar_url?`, `max_events?` |
| `caldav_get_event` | read | allow | `uid` |
| `caldav_create_event` | write | confirm | `summary`, `start`, `end?`, `description?`, `location?` |

The agent reads per-user credentials from the existing
AES-GCM vault (service id `caldav`, fields `url`,
`username`, `password`).

`caldav_list_calendars` is **not** an LLM tool — it is a
setup-only HTTP endpoint
(`POST /api/integrations/caldav/probe-calendars`) that the
integrations UI uses to discover the user's calendar
collection. The chat tool loop returns `"unknown tool"` if
the LLM tries to call it.

## Setup walkthrough

1. Operator: enable the feature at build time
   (`--features nagent-server/caldav-agent`).
2. Operator: set `CALDAV_ALLOWLIST` in the environment (or
   `[agents.caldav].allowlist` in the TOML file). The
   default is empty — every probe call and every chat
   agent call is refused until the operator sets at least
   one host.
3. User: open the Integrations page, click "Add CalDAV",
   paste the principal URL (e.g.
   `https://cloud.example.com/remote.php/dav/calendars/alice/`),
   the username, and the app password. Click "Discover".
   The UI POSTs to the probe endpoint and renders the
   discovered calendars as a dropdown.
4. User: pick a calendar. Click "Save". The chosen
   calendar's `href` is stored as the `url` field in the
   per-user vault.
5. From this point on, every `caldav_*` agent reads
   `ctx.secret("caldav", "url")` and uses it directly as
   the calendar collection URL.

## Configuration knobs

| Knob | Default | Description |
| --- | --- | --- |
| `timeout_ms` | 15 000 | Per-request connect+read timeout. |
| `allowlist` | `[]` | Hostname allow-list (suffix match, `*.foo` glob). Fail-closed when empty. |
| `max_events` | 250 | Cap on `caldav_list_events` payload. |
| `max_body_bytes` | 2 MiB | Cap on `.ics` body size for `get_event` / `create_event`. |

The chat agents and the probe endpoint share the same
allowlist — a misconfigured vault row cannot escape the
operator's host boundary silently because the `CalDavClient`
re-validates the saved `url` on every call.

## Provider notes

- **Nextcloud** — works out of the box. Generate an
  app password (`Settings → Security → App passwords`) and
  use your Nextcloud username. The principal URL is
  `https://cloud.example.com/remote.php/dav/`.
- **Radicale** — works out of the box. Use the per-user
  calendar URL Radicale gives you (typically
  `https://radicale.example.com/alice/`).
- **Fastmail** — works out of the box with the per-calendar
  CalDAV URL Fastmail exposes in the settings page. The
  username is the email address, the password is an
  app-specific token.
- **iCloud** — works but Apple only ships CalDAV with basic
  auth, so generate an app-specific password at
  <https://appleid.apple.com>. The principal URL is
  `https://caldav.icloud.com/`.

## Forbidden operations (v1)

`caldav_update_event` and `caldav_delete_event` are
**explicitly not shipped** in v1:

- The `CalDavClient` does not expose `update()` /
  `delete()` methods, so the plugin cannot emit
  `PUT` over an existing href or `DELETE` requests.
- The `AGENT_DESCRIPTORS` table does not register any such
  agent, so the LLM tool loop returns `"unknown tool"` if
  it tries.
- `caldav_list_calendars` is only reachable through the
  setup HTTP endpoint — never through the chat tool loop.
- Every agent's `description()` ends with the explicit
  message that edit and delete are not supported in this
  version.

This is a deliberate v1 scope cut. The user reviews the
plan; v2 will add edit + delete once the read/add path is
stable and audited.

## Out of scope (deferred to v2)

- Edit / delete (`caldav_update_event`, `caldav_delete_event`).
- Promoting `caldav_list_calendars` to an LLM tool.
- CardDAV (same plumbing, separate tools; deliver after
  this lands).
- `CalDAV-Sync` (`SYNC` REPORT) for incremental refresh.
- Multi-calendar per user (v1 stores a single `url`).
- OAuth2 / OIDC bearer auth (Apple iCloud uses Basic today;
  a future Apple-flow shim is a v2).
- RRULE expansion (the LLM interprets recurrence from the
  raw `RRULE` value).

## Troubleshooting

- **"CALDAV_ALLOWLIST is empty"** — the operator has not
  set the allowlist. The probe endpoint refuses the call
  with 403; the chat agents refuse with
  `SandboxDenied("host … is not in the CalDAV allowlist")`.
- **"CalDAV server rejected the credentials"** — the
  probe endpoint reports this as 502 with the upstream
  body verbatim (not 401), so the UI can show the
  upstream's error message. Common causes: app password
  not generated, username typos, calendar URL pointing at
  a non-CalDAV endpoint.
- **iCal floating times** — the agent normalises
  `DTSTART` / `DTEND` to UTC. Floating times (no `TZID`,
  no `Z`) are interpreted as UTC for v1; if you regularly
  rely on a different floating timezone, file an issue.
