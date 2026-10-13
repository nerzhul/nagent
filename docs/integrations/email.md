# Email integration (IMAP + SMTP)

The email plugin (plan 1791566269874) lets each user connect
their personal IMAP mailbox + SMTP submission server (Fastmail,
iCloud, generic IMAP/SMTP, …) to the chat agent. v1 ships
**five LLM tools + two setup-only probe endpoints**:

- 4 read / flag-set tools: `email_list_inbox`,
  `email_get_message`, `email_search`, `email_mark_read`.
- 1 write tool: `email_send` — the strictest tool in the
  registry, gated by both the inline approval card and the
  operator-set `recipient_allowlist`.

## v1 status

The plan splits the email family into six phases; v1 ships the
**architecture, capability traits, `ServiceDef`s, configs, audit
rows, DB migration, probe endpoints, agent registry wiring, and
the TLS posture** end-to-end. The actual TCP+TLS+IMAP/SMTP wire
layer is a follow-up commit (Phases 3-5 of the plan). Today:

- The probe endpoints validate the host, port, allowlist, TLS
  mode, and the SMTP `from_address` shape; the audit row is
  written with kind `email_<service>_probe_<outcome>`. The
  actual IMAP / SMTP round-trip is a follow-up.
- The five chat agents respond to LLM invocations with a clear
  "wire layer not implemented in v1" error. The capability
  traits, `requires_confirmation` posture, and the
  `recipient_allowlist` server-side gate are stable so the
  follow-up commit can fill the body in without touching the
  agent code.

## Wire surface

| Tool | Kind | Confirmation | Args |
| --- | --- | --- | --- |
| `email_list_inbox` | read | confirm (inline card) | `folder?` (default `INBOX`), `since?` (RFC 3339), `limit?` (default 25, cap 100) |
| `email_get_message` | read | confirm (inline card) | `uid` (IMAP UID, required), `folder?` |
| `email_search` | read | confirm (inline card) | `query` (IMAP SEARCH allow-list), `folder?`, `limit?` |
| `email_send` | write | **always confirm, no session override** | `to`, `subject`, `body`, `cc?`, `bcc?`, `in_reply_to?`, `references?` |
| `email_mark_read` | write | confirm (inline card) | `uid`, `folder?`, `read` (default `true`) |

Every tool routes through the inline approval card machinery
(see `docs/architecture.md` §3.4.1). Reads hit a third-party
service with the user's credentials, so even
`email_list_inbox` is gated — same posture as
`caldav_list_events`.

`email_send` is the strictest tool in the registry: the inline
approval card fires on **every** invocation, and the
`"ApproveAlways for this session"` path is intentionally
**not** wired for it. Sending mail leaves the operator's
perimeter, so the user must click "Autoriser" for each
message — even after they have just confirmed another message
in the same chat session.

`email_send` is also double-gated by a server-side check on
the `to` / `cc` / `bcc` recipients: the operator-set
`[email].recipient_allowlist` is enforced **before** the
agent reaches the network. Default is the user's own
`from_address` only.

## Two `ServiceDef`s, not one

The IMAP and SMTP endpoints for the same mailbox very often
differ (`imap.fastmail.com:993` + `smtp.fastmail.com:465`,
but `imap.gmail.com` + `smtp.gmail.com`, etc.) and the
credentials are not always symmetric (submission sometimes
uses an app-specific password while IMAP uses the real one).
The plan opts for **two `ServiceDef`s**:

- `email_imap` — fields `host`, `port`, `tls_mode`, `username`,
  `password`.
- `email_smtp` — same five fields plus `from_address`.

The two `ServiceDef`s stay distinct and share the operator-set
hostname allow-list (`[email].allowlist`). A future Gmail OAuth
follow-up adds a third `gmail` `ServiceDef` without
conflicting with the IMAP/SMTP pair.

## Setup walkthrough

1. **Operator**: enable the feature at build time
   (`--features nagent-server/email-agent` — the `all-agents`
   meta-feature already includes it).
2. **Operator**: set `EMAIL_ALLOWLIST` in the environment
   (or `[agents.email].allowlist` in the TOML file). The
   default is empty — every probe call and every chat agent
   invocation is refused until the operator configures at
   least one entry.
3. **Operator**: optionally set
   `EMAIL_RECIPIENT_ALLOWLIST` to widen (or, in the default,
   narrow) the outbound recipient set the `email_send` agent
   will accept.
4. **User**: open `/settings/integrations`, pick "Email
   (IMAP)", fill in the IMAP host / port / tls_mode /
   username / password, click "Probe" — the request hits
   `POST /api/integrations/email_imap/probe` and the server
   writes an `email_imap_probe_<outcome>` audit row.
5. **User**: repeat for "Email (SMTP)", adding the
   `from_address`. The SMTP probe validates the
   `from_address` against the server's `MAIL FROM` and writes
   a row to the `email_allowed_from` table on success.
6. **User**: in chat, the LLM can now invoke the five tools.
   Read tools hit the inline approval card on the first call
   in a turn. `email_send` hits the card on every call.

## Hardening

The CalDAV + X patterns define the security posture; the email
agents follow them line by line:

- **Per-service hostname allow-list — split per protocol**:
  the IMAP allow-list (`[email].imap_allowlist` /
  `EMAIL_IMAP_ALLOWLIST`) and the SMTP allow-list
  (`[email].smtp_allowlist` / `EMAIL_SMTP_ALLOWLIST`) are
  **independent**. A read-misconfiguration cannot accidentally
  expose a write path. The two probe endpoints check the right
  list per protocol; the four IMAP-side agents check the IMAP
  list, `email_send` checks the SMTP list. Both lists default
  to empty (fail-closed): every call is refused with
  `AgentError::SandboxDenied` (HTTP 403) until the operator
  sets at least one entry. Reuses
  `nagent_agents::egress::host_matches_allowlist`.
- **TLS posture — IMAP**:
  - `tls_mode = "implicit"` (default) → connect on port 993
    with implicit TLS; agent refuses a `starttls`-only server
    with a clear error.
  - `tls_mode = "starttls"` → connect on port 143, mandatory
    STARTTLS (the client never proceeds without it).
  - `tls_mode = "none"` → refused at save time unless the
    operator sets `[email].allow_cleartext_imap = true`.
- **TLS posture — SMTP**:
  - `tls_mode = "implicit"` (default) → port 465 (implicit
    TLS / "SMTPS").
  - `tls_mode = "starttls"` (default for port 587) → mandatory
    STARTTLS before AUTH.
  - `tls_mode = "none"` → refused for port 25, allowed only
    when the operator sets `[email].allow_cleartext_smtp = true`
    (intended for relay scenarios the operator controls).
- **SMTP sender allow-list** (per-user, server-set):
  `email_send` reads the user's configured `from_address`
  from the vault; the agent never lets the LLM pick one. The
  server validates the value with a one-shot SMTP probe at
  save time (`MAIL FROM:<x>` round-trip per address), caching
  the verdict in the `email_allowed_from` table. No
  LLM-driven outbound mail ever bypasses the row.
- **Capabilities cache**: the IMAP client on first connect
  captures `CAPABILITY` and caches `(idle, condstore,
  search_keys, …)`. `email_search` only sends
  `SEARCH <keys>` when the server advertises
  `SEARCH KEYWORD …` (rare but real — iCloud caps to `ALL`),
  falling back to full-text `SEARCH BODY …` otherwise.
- **Audit row per call**: every agent writes one
  `auth_events` row of kind `email_<tool>_<outcome>` with
  `target_service = "email_imap" | "email_smtp"`; the per-call
  wire bytes are **never** captured (RFC 5322 bodies leak
  PII), only the host + outcome + at most a 256-byte
  message-id hash. The secret resolver already audits
  credential access on the read path; the agent duplicates
  one write so the row carries the tool name explicitly.

## Operator configuration surface

The `[email]` TOML section (and its `EMAIL_*` env-var
counterparts) covers the full protocol + security surface.
Operators set the policy; users only see the form.

| Knob | Type | Default | Description |
| --- | --- | --- | --- |
| `imap_allowlist` | CSV | `[]` | Hostname allow-list for **IMAP** servers (read side). Fails closed when empty. |
| `smtp_allowlist` | CSV | `[]` | Hostname allow-list for **SMTP** servers (write side). Fails closed when empty. Independent of the IMAP list. |
| `timeout_ms` | u64 | 15000 | Per-request connect+read timeout. |
| `max_messages_per_call` | usize | 100 | Cap on `email_list_inbox` / `email_search`. |
| `max_body_bytes` | usize | 2 MiB | Cap on RFC 5322 body the LLM ever sees. |
| `max_attachment_bytes` | usize | 0 (deny in v1) | Cap on attachment bytes. |
| `allow_cleartext_imap` | bool | false | Permits `tls_mode = "none"` for IMAP. |
| `allow_cleartext_smtp` | bool | false | Permits `tls_mode = "none"` for SMTP. |
| `strict_tls` | bool | true | Refuses self-signed certs regardless of the user form. |
| `recipient_allowlist` | CSV | `[]` | Operator-set outbound `to` / `cc` / `bcc` allow-list for `email_send`. Empty = the user's own `from_address` only. |
| `default_probe_helo` | string | `nagent.local` | `EHLO` identity used by the SMTP probe. |

### Common allow-list postures

- **Personal assistant, broad reads, locked writes**:
  `imap_allowlist = ["*"]` (or any host the user can type)
  with `smtp_allowlist = ["smtp.corporate.example"]` so the
  user can read from any IMAP mailbox they connect to, but
  outgoing mail is forced through the corporate relay.
- **Locked reads, locked writes** (single corporate tenant):
  set both lists to the same single host.
- **Read-only deployment** (e.g. a triage bot that never
  sends): `smtp_allowlist = []` — the empty list refuses
  every `email_send` call, so a misconfigured vault cannot
  even attempt a send.

## Search query safety

`email_search`'s `query` is constrained to a hand-rolled token
allow-list: `ALL`, `SUBJECT`, `FROM`, `TO`, `BODY`, `SINCE`,
`BEFORE`, `UID`, `UNSEEN`, `SEEN`. The agent rejects anything
else with `AgentError::InvalidArguments`. Free-form
Gmail-style queries (`from:alice subject:report`) are
deliberately v2 — implementing a proper query parser would
mean re-importing the iCloud / Gmail search grammar, which
is out of scope for this plan.

## Provider notes

- **iCloud**: ships plain-text IMAP with App-Specific
  Passwords. The `email_imap` `ServiceDef` defaults
  (`port = 993`, `tls_mode = implicit`, `host = imap.mail.me.com`)
  match the iCloud setup.
- **Fastmail**: `imap.fastmail.com:993` + `smtp.fastmail.com:465`,
  `tls_mode = implicit` for both.
- **Gmail**: requires an app-specific password (or OAuth — v2).
  The IMAP / SMTP hosts and ports match the Fastmail pair.

## Out of scope (deferred to v2)

- **Gmail OAuth** / provider-specific flows. The `gmail`
  `ServiceDef` and the OAuth plumbing are a follow-up plan.
- **Attachments** (`email_get_message` returns metadata only;
  `email_send` does not yet accept `attachments`).
- **Folder management** (`email_create_folder`,
  `email_move_message`).
- **IDLE / push notifications** (real-time "new mail" pings).
- **S/MIME / PGP signing**.
- **Sync state cache** across sessions (every list / search
  round-trips the IMAP server).

## See also

- `docs/architecture.md` §2.4 (built-in agents) and §2.6
  (services).
- `docs/integrations/caldav.md` — sibling per-user integration
  with the same probe + audit pattern.
- `docs/integrations/x.md` — sibling per-user integration
  with the same per-call connection + capability trait
  pattern.
