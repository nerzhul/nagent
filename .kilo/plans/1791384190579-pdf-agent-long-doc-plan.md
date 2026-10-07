# PDF agent: long-document analysis on a constrained environment

## Goal

Make the `read_document` agent usable for long-document analysis in a
small-local-LLM deployment (Ollama with limited `num_ctx`, no vector store,
no embeddings, no extra API calls). Concretely:

1. **Fix the wrong page count** — the bubble currently shows `? pages` for
   every PDF because the extractor hardcodes `page_count: None`.
2. **Fix the "memory loss" perception** — today `read_document` re-parses
   the PDF on every call, then injects up to 100 000 chars of verbatim text
   into a single `role: "tool"` message. On a small Ollama with the default
   2 048-token `num_ctx` (and even on 8–32 k), the previous turns are
   pushed out of the context window the moment one full document lands in
   a tool result. The model is not losing memory — the upstream is
   truncating the conversation.
3. Keep the change compatible with the **no-extra-LLM-call** constraint
   (summarisation, embeddings, RAG all out of scope).

## Approach (decisions taken during planning)

- **Storage layout**: per-page text lives **on disk** (one file per page)
  next to the existing PDF bytes, **encrypted at rest with the same
  AES-256-GCM `CredentialsKey` already used by `memory_store`** (per user
  answer). The DB keeps only the metadata: real `page_count`, the path to
  the pages directory, a short `preview` (first ~2 000 chars, also
  encrypted), and an optional extracted `table_of_contents`.
- **Default `read_document(name)` (no `page_range`)** returns an
  **overview only** (per user answer): `page_count`, `size_bytes`,
  `mime`, `extracted_chars`, an optional TOC, and a `preview`. The LLM
  is instructed by both the tool description and the result envelope to
  make subsequent calls using `page_range` to fetch specific pages.
- **`read_document(name, page_range)`** returns only the requested pages,
  capped at `max_pages_per_call` and `max_page_chars_per_call`. This is
  the working knob that keeps each `role: "tool"` result small enough that
  a long analysis does not blow the upstream context.
- **Extraction runs once at upload** and is never re-parsed on read. The
  upload route becomes authoritative for `page_count` and per-page text.
- **Encryption key reuse**: `read_document` is gated on
  `state.credential_encryption_key().is_some()`. If the key is absent
  (degraded mode, same as `memory_source` today), the upload is refused
  with a clear error explaining the missing `[auth.credentials].key`.
  No new key material, no new env var, no new config flag is required —
  the encryption contract mirrors `UserDbMemorySource` exactly.

## Current state (relevant findings)

- `crates/nagent-agents/src/agents/read_document.rs:114-123` — the
  bubble's `summary` shows `?` because `payload.page_count` is always
  `None`.
- `crates/nagent-server/src/documents/extract.rs:39-52, 113-119` — both
  branches of the extractor hardcode `page_count: None`.
- `crates/nagent-server/src/agents/mod.rs:151-309` — `StoreDocumentSource::read`
  re-runs the full PDF parser on every `read_document` call, with no
  per-page state anywhere.
- `crates/nagent-server/src/llm/tool_loop.rs:561-570` — the entire
  `payload` string (up to 100 000 chars) is appended verbatim to
  `messages` as `role: "tool"`.
- `crates/nagent-agents/src/agents/read_document.rs:63` —
  `page_range` is parsed+validated but ignored ("advisory").
- `crates/nagent-server/Cargo.toml:134-135` — `pdf-extract = "0.7"`
  is already there; `lopdf` is transitive on it and can be made a
  direct dependency without duplicating the version.
- `crates/nagent-server/src/credentials/crypto.rs:52, 77` — reusable
  `encrypt` / `decrypt` for the shared `CredentialsKey`.
- `crates/nagent-server/src/state.rs:296-301` — accessor for the
  shared `Arc<CredentialsKey>`.
- `crates/nagent-server/src/app.rs:496-511` — the gating pattern to
  mirror (`build_memory_source`).

## Files to add / modify

### New / moved

- `crates/nagent-server/src/documents/pages.rs` — new module:
  - `pub struct PagesIndex { pub page_count: Option<u32>, pub pages_dir: PathBuf, pub preview: String, pub toc: Vec<TocEntry> }`
  - `pub fn extract_pages(bytes: &[u8], out_dir: &Path, key: &CredentialsKey) -> Result<PagesIndex, ExtractionError>` — opens with `lopdf::Document::load`, iterates `get_pages()`, calls `extract_text(&[page_id])` per page, writes each page's text encrypted at `<out_dir>/page-NNNN.bin` (`nonce(12) || ciphertext+tag(16)`), writes a `meta.bin` (`nonce(12) || ciphertext+tag(16)`) wrapping a JSON `{ page_count, preview, toc }`.
  - `pub fn read_pages(dir: &Path, key: &CredentialsKey, range: PageRange) -> Result<Vec<String>, ReadError>` — decrypts only the requested page files.
  - `pub fn read_overview(dir: &Path, key: &CredentialsKey) -> Result<PagesIndex, _>` — decrypts only `meta.bin`.
  - `pub struct PageRange { pub start: u32, pub end_inclusive: u32 }` with `pub fn parse(s: &str, max: u32) -> Result<Self, _>`.
- `crates/nagent-server/src/documents/extract.rs` — slim down: keep
  the text-passthrough branch for `.txt/.md/.log`; delete the PDF
  branch (now lives in `pages.rs`). Re-export `extract_text` and the
  `ExtractionResult { text, page_count: Option<u32>, mime }` for
  non-PDF files (still used by tests).

### Modified

- `crates/nagent-server/Cargo.toml` — add direct dependency on
  `lopdf = "0.x"` (same minor as `pdf-extract`'s transitively pinned
  version; check `Cargo.lock` before pinning).
- `crates/nagent-server/src/agents/mod.rs` —
  - `StoreDocumentSource::read` (lines 151-309) becomes: load the
    `pages_dir` from the DB row, call `pages::read_overview` if no
    `page_range`, otherwise `pages::read_pages` for the requested
    range, then build the JSON envelope from `PagesIndex` +
    `page_range`. No re-parsing.
  - The `DocumentPayload` returned to the agent no longer carries the
    full text — it carries `overview: DocumentOverview`, or
    `pages: Vec<PageText>` for a range. Add to
    `crates/nagent-agents/src/agents.rs:144-181` (`DocumentSource`
    trait) a way to pass the parsed `PageRange` through `UserContext`
    or as an extra read-arg; simplest: extend `DocumentSource::read`
    signature to accept `Option<PageRange>` (or a thin new struct
    `DocumentReadRequest { name, page_range }`). This is a breaking
    change to the trait but the trait has a single implementer in
    the tree today.
- `crates/nagent-agents/src/agents/read_document.rs` —
  - `parameters_schema` (lines 53-69): update the description of
    `page_range` from "advisory" to "restrict to a page range; the
    tool returns at most `max_pages_per_call` pages or
    `max_page_chars_per_call` characters — fetch additional ranges
    with further calls". Drop the `max_extracted_chars` mention
    since the tool now caps per-call, not per-document.
  - `invoke` (lines 94-137): branch on whether `page_range` is set.
    Overview mode → JSON with `summary` showing real `page_count`,
    `data` carrying `{ page_count, preview, toc, ... }` (no
    `data.text`). Range mode → JSON with `summary` showing
    `pages X-Y of N`, `data.text` carrying the joined pages with
    per-page separators (`\n\n--- page 7 ---\n...\n\n--- page 8 ---\n...`),
    plus `data.page_range_applied`. Either mode returns a one-line
    `data.hint` reminding the model to use `page_range` to fetch
    additional content.
  - `parse_args` (lines 164-204) keeps its current validation;
    `PageRange` is built in `StoreDocumentSource::read` instead.
- `crates/nagent-server/src/documents/routes.rs` —
  - Upload handler (lines 425-466): refuse the upload if
    `credential_encryption_key().is_none()` (mirror
    `build_memory_source`).
  - After persisting the PDF bytes, call `pages::extract_pages` to
    write `<shard>/<bb>/<uuid>/pages/` directory with one
    `page-NNNN.bin` per page plus `meta.bin`.
  - Update the `uploaded_documents` row INSERT (lines 458-466) to
    store the real `page_count`, `extracted_chars`, a `pages_dir`
    column, and the encrypted `preview_nonce` + `preview_ciphertext`
    columns (or stash the encrypted `preview` inside `meta.bin`
    only and not mirror it in the DB — decide during
    implementation; default: keep `meta.bin` self-contained, no
    DB-side preview duplication).
  - Migration `crates/nagent-db/migrations/00NN_documents_pages.up.sql`
    (new): `ALTER TABLE uploaded_documents ADD COLUMN pages_dir TEXT;`
    + corresponding `.down.sql`. Bump the migration counter to the
    next free number when implementing.
- `crates/nagent-server/src/state.rs` — no signature change needed;
  the `Arc<CredentialsKey>` is already reachable. The new
  `StoreDocumentSource` takes an `Arc<CredentialsKey>` alongside its
  existing `Arc<dyn DocumentStore>` (and `Arc<ChatSessionsStore>`).
- `crates/nagent-server/src/app.rs` — pass the `Arc<CredentialsKey>`
  into the `StoreDocumentSource` constructor. Same gating as
  `build_memory_source` (line 496): if `credentials_key.is_none()`
  the documents subsystem stays disabled at boot (matches today's
  graceful-degradation contract).
- `crates/nagent-server/src/config/documents.rs` — add two new
  fields with the existing `resolve_primitive` plumbing:
  - `max_pages_per_call` — env `DOCS_MAX_PAGES_PER_CALL`, TOML
    `[documents].max_pages_per_call`, default `20`.
  - `max_page_chars_per_call` — env `DOCS_MAX_PAGE_CHARS_PER_CALL`,
    TOML `[documents].max_page_chars_per_call`, default `20_000`.

### Tests

- `crates/nagent-server/tests/documents.rs` —
  - Update `read_document_pdf_round_trips_through_extractor`
    (line 192) to assert the new overview envelope (real number, no
    `data.text`, presence of `toc` and `preview`).
  - Add `read_document_pdf_returns_real_page_count` — load a known
    PDF, assert `summary` shows `N pages` (not `?`).
  - Add `read_document_pdf_page_range_returns_only_target_pages` —
    assert `read_document(name, page_range="3-5")` returns only pages
    3, 4, 5 text and that the response size stays under
    `max_pages_per_call * max_page_chars_per_call`.
  - Add `read_document_pdf_page_range_exceeds_cap_is_rejected` —
    range wider than `max_pages_per_call` returns
    `InvalidArguments` with the cap echoed in the message.
  - Add `read_document_pdf_pages_encrypted_at_rest` — assert
    `<pages_dir>/page-0001.bin` is not parseable as UTF-8 and starts
    with a valid 12-byte nonce (defence-in-depth check that
    encryption is active).
  - Add `read_document_without_credentials_key_returns_503` —
    upload and read without `credentials_key` set, assert the
    route refuses the upload and the agent (if invoked) errors
    cleanly.
  - Existing `read_document_truncates_long_text` (line 417) is
    obsolete — replace with `read_document_overview_caps_preview`
    asserting the preview never exceeds `2 000` chars.
  - All other tests (`unknown_id`, `other_session_scope`,
    `file_missing_on_disk`, `without_session_id`, `invalid_page_range`,
    `rejects_malformed_uuid`, `blocks_cross_user_reads`,
    `blocks_disk_path_escape`) remain valid; rerun them after the
    `DocumentSource::read` signature change.
- `crates/nagent-server/src/documents/pages.rs` — add unit tests
  for `PageRange::parse` (happy + 5 bad inputs), `extract_pages`
  on a 1-page synthetic PDF, and `read_pages` round-trip with a
  real PDF fixture in `tests/fixtures/`.
- `crates/nagent-server/src/documents/extract.rs` — keep the
  existing tests (`txt_round_trip`, `lossy_utf8`, `unknown_ext`,
  `bounded_pdf_respects_semaphore`) and remove the PDF branch
  test (now lives in `pages.rs`).

### Documentation sync (per AGENTS.md §5)

- `docs/configuration.md` (and `docs/architecture.md` if it lists
  `[documents]` knobs) — document `max_pages_per_call`,
  `max_page_chars_per_call`, the new `pages_dir` column, and the
  `[auth.credentials].key` precondition for the documents
  subsystem.
- `docs/tools.md` / `docs/agents.md` (whichever documents the
  `read_document` tool) — update the JSON shape returned by the
  tool, the `page_range` semantics (no longer advisory), and the
  new overview-mode contract.
- `README.md` — if it lists `DOCS_*` env vars, add the two new
  ones; if it lists `[documents]` TOML keys, same.
- Tool description string in `crates/nagent-agents/src/agents/read_document.rs`
  is part of the contract; update accordingly (already in the file
  list above).

## Validation plan

- `cargo fmt` (zero diff).
- `cargo build --all-targets --all-features` (zero warnings).
- `cargo clippy --all-targets --all-features -- -D warnings`.
- `cargo test -p nagent-server` (covers the existing 11
  `documents.rs` tests, the new tests, and the unit tests in
  `extract.rs` / `pages.rs`).
- `cargo audit`.
- Manual smoke (optional, documented in commit body): upload the
  repo's `five_steps_perform_2009.pdf` (≈412 KB, ~42 pages) and
  verify (a) the bubble shows `42 pages` (real count), (b) the
  first `read_document(name)` returns the overview only, (c)
  `read_document(name, page_range="20-25")` returns 6 pages,
  (d) `read_document(name, page_range="1-100")` is rejected by
  the cap with a clear error.

## Out of scope (called out explicitly)

- **OCR / scanned PDFs** — `lopdf`/`pdf-extract` only extract the
  text layer. A scanned-only PDF still returns the empty string
  and `page_count` from the page tree (the page count fix
  benefits scanned PDFs even if the text is empty).
- **Vector store / embeddings / RAG** — the user picked "no extra
  infra".
- **Summarise-on-upload via a second LLM call** — same reason.
- **Persistent session-scoped working-memory notes** that summarise
  each tool result so future turns can `memory_recall` instead of
  re-fetching — useful but a larger feature; mention in the commit
  body as a follow-up.
- **Per-page images / tables** — `pdf-extract` does not emit them,
  and adding `pdfium-render` is a large change.
- **Auto-detection of the LLM's `num_ctx` to choose the cap** —
  keep the cap operator-configurable for now.
- **Server-side replay of conversation history** — unrelated to
  this plan.

## Risks

- The `DocumentSource::read` signature change touches every
  implementer in the workspace; only `StoreDocumentSource`
  implements it today, but the trait lives in `nagent-agents`
  and any downstream agent that reads documents would break.
  Mitigation: keep the old method, add a new
  `read_with_request(DocumentReadRequest)` default-impl that calls
  the old one ignoring the range. Deprecate the old method in a
  follow-up.
- AES-GCM nonce reuse — `pages::extract_pages` and
  `pages::read_overview` use a fresh OS-RNG nonce per file (same
  helper as `credentials/crypto.rs:rand_bytes_12`); per-page and
  per-meta nonces are independent, no reuse.
- Disk growth — each page stored encrypted at rest; for a
  1 000-page document the per-page overhead is ~28 bytes (12-byte
  nonce + 16-byte GCM tag) per page, negligible. Document this in
  `docs/operations.md` if it exists.
- Migration on existing rows — `ALTER TABLE uploaded_documents
  ADD COLUMN pages_dir TEXT` is non-destructive; existing rows
  have NULL `pages_dir`. `StoreDocumentSource::read` must check
  the column and, for legacy rows, fall back to on-the-fly
  re-extraction (no encryption key needed for those because the
  PDF bytes are still on disk unencrypted — same as today). Mark
  the legacy path as deprecated in a comment.