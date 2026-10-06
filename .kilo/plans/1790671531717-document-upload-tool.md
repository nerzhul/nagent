# Plan — Document upload + `read_document` LLM tool

## Goal

Let a user upload `.txt` and `.pdf` files in the Discussion-mode UI and
expose them to the local LLM through a server-side `read_document` tool,
so the model can pick which document (or page range) to consult instead
of receiving every document passively on every turn.

## User-facing behavior

1. A new **Documents** panel lives in the left sidebar of the Discussion
   view, above (or below) the existing chat-session list. Each entry
   shows: original filename, size, page count (PDFs only), mime badge,
   and a delete (×) button.
2. The user uploads by **any** of: clicking the panel's `+` button
   (opens a file picker), dragging one or more files onto the panel,
   or pasting a file from the OS clipboard (`Ctrl+V`).
3. Each upload streams to the server, gets a UUID-backed disk path under
   a configurable cache directory, and an `uploaded_documents` row is
   written linking the UUID to the original filename + extracted text.
4. The LLM sees a `read_document` tool in its `tools` array. Its
   description lists the available documents by name + size. The LLM
   decides when to call it. Returned text is appended to the
   conversation as a normal `role: "tool"` message — same plumbing as
   `web_fetch` today.
5. A `nagent documents purge --older-than <DURATION>` CLI subcommand
   sweeps both the DB rows and the orphaned files on disk.

## Architecture (one diagram)

```
Browser                                  Server                              Disk / DB
--------                                 ------                              ---------
sidebar.drag/drop/paste     ─POST─► /v1/documents (multipart)   ─write─►   /var/cache/nagent/docs/ab/<uuid>.pdf
chat.js includes                         extract (pdf-extract)              uploaded_documents row
  X-Chat-Session-Id           ─POST─► /v1/chat/completions       ─insert─  uploaded_documents.session_id = X-Chat-Session-Id
LLM decides to call                   tool loop sees tool_call
  read_document(name=...)  ─run─►     agents::ReadDocumentAgent            read file from disk; parse page range
                                       returns JSON {pages, text}          if missing → AgentError::AgentFailed("file purged")
sidebar × delete           ─DELETE─► /v1/documents/{id}          ─unlink─  file + DB row
CLI nagent documents purge ──────►   sweep_old_documents()               unlink file + delete row
```

## Affected files

### New (server)
- `crates/stt-server/src/documents/` — new module:
  - `mod.rs` — public types, `DocumentStore` wrapper
  - `storage.rs` — disk layout, UUID sharding, write/read/unlink
  - `extract.rs` — text extraction (`.txt` passthrough, `.pdf` via
    `pdf-extract`)
  - `agent.rs` — `ReadDocumentAgent` (registered like the others in
    `AgentRegistry::from_config`)
  - `routes.rs` — `POST /v1/documents`, `DELETE /v1/documents/{id}`,
    `GET /v1/documents` (list), `GET /v1/documents/{id}` (download)
  - `purge.rs` — `purge_older_than(Duration)` helper used by CLI +
    a periodic background task
- `crates/stt-server/src/bin/nagent_documents.rs` (or merged into the
  existing CLI binary if there is one) — `purge` subcommand. Decision:
  fold into the main `nagent` CLI binary the project already exposes.
- `crates/stt-server/migrations/NNNN_uploaded_documents.sql` — schema
- `crates/stt-server/src/static/documents.js` — new client module
  (panel, upload, drag-drop, paste)

### Modified (server)
- `crates/stt-server/src/lib.rs` — mount `/v1/documents*` routes,
  wire the `DocumentStore` into `AppState`
- `crates/stt-server/src/config.rs` — new `[documents]` table
  (`enabled`, `cache_dir`, `max_file_size_bytes`,
  `max_extracted_chars`, `max_docs_per_session`, `purge_interval_hours`,
  `pdf_extract_timeout_secs`)
- `crates/stt-server/src/main.rs` — initialise the `DocumentStore` at
  boot, run migrations, schedule the periodic purge task, log the
  resolved cache dir
- `crates/stt-server/src/agents/mod.rs` — `UserContext` gains an
  optional `chat_session_id: Option<Uuid>`; add a constructor
  `for_chat_session(session_id, user_id, services)`. Tools that need it
  read it via `ctx.chat_session_id()`. No behaviour change for
  existing tools.
- `crates/stt-server/src/llm.rs` — when `chat_session_id` is present
  in the request body (new optional field), thread it into the
  `UserContext` built for each tool round. Also surface it in the SSE
  `tool_call` event so the browser can show "used doc X".
- `crates/stt-server/src/agents/services.rs` — no change expected; if
  the doc tool needs to know the user as well, plumb it via the same
  `user_id` already on `UserContext`.

### Modified (frontend)
- `crates/stt-server/src/static/index.html` — add `<aside
  id="documents-panel">` next to the chat sidebar, and the `<input
  type="file">` it triggers
- `crates/stt-server/src/static/chat.js` — import the new
  `documents.js`, refresh the doc list on session switch, send the
  active session id as `X-Chat-Session-Id` on every chat-completions
  request (header, not body — headers are easier to validate and
  survive JSON re-serialisation)
- `crates/stt-server/src/static/style.css` — styles for the panel,
  drag-over highlight, empty state, badge colours
- `crates/stt-server/src/static/chat-sessions.js` — no change; doc
  storage is server-side, the session UUID stays in `localStorage`

### Modified (infra)
- `deploy/k8s/base/*.yaml` — add a `PersistentVolumeClaim` named
  `nagent-docs-cache`, mounted at `/var/cache/nagent/docs` in the
  server pod
- `deploy/k8s/overlays/*` — update the corresponding patches if any
  pin volumes
- `docs/examples/config.toml.example` — add the `[documents]` block
  with the new defaults
- `README.md` — add a `## Documents` section mirroring the `Chat`
  section structure (quick start, configuration, agent description,
  CLI, kustomize note)
- `crates/stt-server/Cargo.toml` — add `pdf-extract = "0.7"` (or
  `lopdf` if `pdf-extract` has unmaintained transitive deps — to be
  confirmed during implementation), and `mime_guess = "2"` for
  content-type detection
- `Makefile` — add the `--features stt-server/documents` flag to
  `make run-llm` (and only that target, to keep the GPU builds lean)

## Backend design details

### DB schema (`uploaded_documents`)

| Column           | Type         | Notes                                           |
| ---------------- | ------------ | ----------------------------------------------- |
| `id`             | `BLOB(16)`   | UUIDv4, primary key                             |
| `session_id`     | `BLOB(16)`   | UUID from `X-Chat-Session-Id` header            |
| `user_id`        | `BLOB(16)?`  | from auth context when `auth.enabled`           |
| `original_name`  | `TEXT`       | not unique, sanitised for display only          |
| `mime`           | `TEXT`       | sniffed via `mime_guess` + extension           |
| `size_bytes`     | `INTEGER`    | on-disk size                                    |
| `extracted_chars`| `INTEGER`    | total chars extracted                           |
| `page_count`     | `INTEGER?`   | `NULL` for `.txt`                               |
| `disk_path`      | `TEXT`       | relative to `cache_dir`                         |
| `created_at`     | `TIMESTAMP`  | defaults to `now()`                             |
| `expires_at`     | `TIMESTAMP?` | optional admin override; purge honours it       |
| index            | `(session_id, created_at DESC)`              | for the sidebar list                            |

Migration is `IF NOT EXISTS`-guarded so re-running the boot sequence is
safe.

### Disk layout

```
<cache_dir>/
  ab/
    cd/
      abcd1234-....pdf   # 2-char + 2-char sharding under the UUID
  ef/
    gh/
      efgh5678-....txt
```

The two-level shard is computed from the first four hex chars of the
UUID (2 chars per level), matching the user's stated arborescence.
Sharding caps any single directory at ~65k entries (UUIDs are random,
so collisions on the leading chars are rare).

### REST endpoints

| Method | Path                       | Behaviour                                | Auth      |
| ------ | -------------------------- | ---------------------------------------- | --------- |
| POST   | `/v1/documents`            | multipart upload, returns JSON `{id,...}`| required  |
| GET    | `/v1/documents`            | list docs for `X-Chat-Session-Id`        | required  |
| GET    | `/v1/documents/{id}`       | download original (Content-Disposition)  | required  |
| DELETE | `/v1/documents/{id}`       | unlink + delete row                      | required  |

All four endpoints share the LLM CORS / rate-limit envelope
(`LLM_CORS_ALLOW_ORIGINS`, `LLM_RATE_PER_MIN`) and the
`LLM_AUTH_MODE` gate so a misconfigured production server behaves
identically to the existing `/v1/*` routes. Auth comes from the
existing `RequireAuth` middleware when `auth.enabled = true`.

### `read_document` tool schema

```json
{
  "name": "read_document",
  "description": "Read the text content of a document previously uploaded by the user to this chat session. Use the `name` field returned by GET /v1/documents. For PDFs, optionally restrict to a `page_range` (e.g. \"3-7\") to limit context size.",
  "parameters": {
    "type": "object",
    "properties": {
      "name":  { "type": "string", "description": "Document id (UUID)" },
      "page_range": { "type": "string", "description": "Optional, format 'N' or 'N-M'" }
    },
    "required": ["name"]
  }
}
```

The tool reads `ctx.chat_session_id()` (panicking with a clear error
if `None` — direct `/v1/agents/read_document/invoke` is rejected at
the route layer with `400`). It looks the doc up scoped to that
session id, so a doc uploaded in session A is invisible to session B.

Failure modes the tool surfaces (mapped to `role: "tool"` errors so
the LLM can recover):

- `name` not found in this session → `unknown document`
- file on disk was purged between upload and call → `document file no
  longer available on disk; ask the user to re-upload`
- `page_range` malformed or out of range → `invalid page range`
- PDF parse error → `pdf extract failed: <reason>`

### Configuration

```toml
[documents]
enabled = true                        # master switch
cache_dir = "/var/cache/nagent/docs"  # must be writable; PVC mount in k8s
max_file_size_bytes = 20 * 1024 * 1024
max_extracted_chars = 100_000
max_docs_per_session = 50
pdf_extract_timeout_secs = 30
purge_interval_hours = 24             # background sweep
default_ttl_days = 30                 # rows older than this are purged
```

All keys mirror the existing `[llm]` / `[agents]` style and land in
`Config` via the same `Config::from_env_and_file` plumbing.

### CLI subcommand

```
nagent documents purge --older-than 30d [--dry-run] [--config <path>]
```

Honours the same env / `--config` overrides as the server. `--dry-run`
prints the matched rows without touching the DB or the disk. Exit
code is non-zero on any unlink / DB error so it can be wired to
`cron` / k8s `CronJob`.

## Frontend design details

### Sidebar panel

- Header row: title `Documents`, `+ Upload` button (opens picker).
- Empty state: hint text `Drag a PDF or text file here, or paste one.`
- List rows: filename, mime badge, size (`1.2 MB`), page count for
  PDFs (`12 pages`), delete button. Clicking the row opens a preview
  tooltip with the first 200 chars of the extracted text.
- Drag-over state: a 2-px dashed border highlight, `cursor: copy`.
- Paste handler on `window`: if `e.clipboardData.files.length > 0`,
  forward each file to the upload pipeline (skip if focus is in an
  editable element so we don't hijack text paste in the chat input).

### Refresh strategy

- After every successful upload: prepend the returned entry to the
  list.
- After every delete: remove the row with a fade-out.
- On session switch (`loadHistory` / sidebar click): re-fetch the
  list scoped to the now-active session id.
- Initial load: parallel to the existing `GET /v1/agents` probe.

### Session-id propagation

`chat.js` already holds the active session UUID in memory (it's how
`appendBubble` and `streamReply` correlate). A single change adds
`X-Chat-Session-Id: <uuid>` to the `fetch()` headers for `/v1/*`
calls. The backend reads it in `chat_completions` and `documents`
handlers via `axum::TypedHeader<HeaderMap>`.

## Limits & defaults (parametrable)

| Setting                  | Default | Override env            |
| ------------------------ | ------- | ----------------------- |
| Max file size            | 20 MiB  | `DOCS_MAX_FILE_BYTES`   |
| Max extracted chars      | 100 000 | `DOCS_MAX_CHARS`        |
| Max docs per session     | 50      | `DOCS_MAX_PER_SESSION`  |
| PDF parse timeout        | 30 s    | `DOCS_PDF_TIMEOUT_SECS` |
| Background purge cadence | 24 h    | `DOCS_PURGE_INTERVAL_H` |
| Default TTL              | 30 d    | `DOCS_TTL_DAYS`         |

A file that exceeds the extracted-char cap is still stored; the tool
returns the first N chars and a `[... truncated ...]` marker.

## Failure modes & edge cases

- **File missing on disk when tool runs** — DB row survives an admin
  purge of just the file; the tool returns a tool-error so the LLM
  tells the user. The row is kept (with a `file_missing=1` flag set
  by the periodic sweep) so the UI can show a `⚠` badge and offer a
  re-upload.
- **Session id mismatch** — request body has `X-Chat-Session-Id`
  but `auth.enabled = true` and the session cookie resolves to a
  different user → 403. With `auth.enabled = false` (single-user
  trust), the id is trusted as-is.
- **Two uploads with the same original name** — no collision; both
  rows coexist, both shown in the sidebar with the same display name.
- **Concurrent uploads from two browser tabs** — per-session counter
  rejects the 51st; existing rows are untouched.
- **pdf-extract timeout** — upload fails with `422`; partial file is
  unlinked. The sidebar shows an error toast.
- **PVC full** — write fails with `ENOSPC`; HTTP `507 Insufficient
  Storage`; UI suggests deleting old docs.
- **Cache dir not writable at boot** — server refuses to start with a
  clear error (`[documents] cache_dir /var/cache/nagent/docs is not
  writable`). The `enabled = false` carve-out keeps the rest of the
  server usable.

## Validation

### Unit tests (Rust)

- `documents::storage::path_for_uuid` sharding correctness
- `documents::extract::extract_text` happy-path on synthetic PDF + TXT
- `documents::agent::ReadDocumentAgent::invoke` for: hit, miss,
  page-range clipping, file-missing-on-disk, malformed `page_range`
- `llm::strip_user_location_does_not_touch_…` analogue for the new
  session-id injection (no leakage between sessions)
- CLI `purge` with a synthetic DB fixture: counts, dry-run, error path

### Integration tests (Rust)

- `tests/documents.rs` — boot a test `AppState`, upload a small PDF
  via `axum::body::Multipart`, list, read, delete. Re-uses the same
  test harness as `tests/agents.rs`.

### Frontend tests

- Extend `scripts/verify-chat-sessions.ts` with a doc-pane smoke
  check (mocked `fetch`).
- Manual: open the Discussion view, upload a 50-page PDF, ask the
  model "summarise page 7", confirm the `read_document` tool bubble
  appears in the timeline and the answer references page 7 content.

### Manual smoke checklist (before merge)

- [ ] Upload `.txt`, upload `.pdf`, upload `.png` → last one rejected
      with a clear error mentioning supported types.
- [ ] Drag-and-drop a folder of mixed types → only supported ones
      processed, others listed as errors in a toast.
- [ ] Paste a PDF from the OS clipboard → uploaded, listed.
- [ ] Switch chat session → doc list refreshes, scoped correctly.
- [ ] Delete a doc, then ask the LLM to read it → tool returns
      `document no longer available`.
- [ ] Run `nagent documents purge --older-than 1s --dry-run` → lists
      recent rows; without `--dry-run` → unlinks files + deletes
      rows; subsequent boot leaves the cache dir empty.

## Out of scope (explicit)

- **Image / vision inputs.** Text + PDF only. Adding later is a
  separate plan (would require the OpenAI multimodal `content`
  array, a `vision` model on the Ollama side, and a `read_image`
  agent).
- **Vector search / embeddings.** The LLM picks the document by name
  only. Chunking + similarity search is a follow-up.
- **Per-user quotas when `auth.enabled = false`.** The single-user
  trust boundary keeps the limit at "per session".
- **Server-side redaction / PII stripping.** Out of scope; the user
  uploads at their own risk, same as today's clipboard paste.

## Open questions

None remaining. The plan is implementation-ready pending user
approval to commit.
