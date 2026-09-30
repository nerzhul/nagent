-- 0003_uploaded_documents.up.sql — Discussion-mode document uploads.
--
-- Reversed by `0003_uploaded_documents.down.sql`; keep the two
-- siblings in sync (sqlx 0.8.6 pairs them via the
-- `<version>_<name>.up.sql` / `<version>_<name>.down.sql` filename
-- convention).
--
-- One row per upload. The server stores the original bytes at
-- `<documents.cache_dir>/<shard>/<shard>/<uuid>.<ext>` and reads
-- them back when the `read_document` tool runs. `extracted_chars`
-- is the number of characters parsed at upload time (used to
-- report progress + to size the agent's truncation marker); for
-- PDFs `page_count` is the lopdf page count. `expires_at` is an
-- optional operator-set override; the periodic sweep honours it
-- alongside `created_at + default_ttl_days`.
--
-- `session_id` is the UUID from the `X-Chat-Session-Id` header sent
-- by the browser on every `/v1/*` request; the server never sees
-- the user's session cookie here, so the documents table is
-- decoupled from `sessions` and does not require a join to query.
-- `user_id` is set when `auth.enabled` is true (best-effort); it
-- stays NULL in the single-user trust boundary so the table keeps
-- working without the auth subsystem.

CREATE TABLE IF NOT EXISTS uploaded_documents (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    user_id         TEXT,
    original_name   TEXT NOT NULL,
    mime            TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL,
    extracted_chars INTEGER NOT NULL DEFAULT 0,
    page_count      INTEGER,
    disk_path       TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at      TEXT
);

CREATE INDEX IF NOT EXISTS uploaded_documents_session_idx
    ON uploaded_documents(session_id, created_at DESC);

CREATE INDEX IF NOT EXISTS uploaded_documents_created_at_idx
    ON uploaded_documents(created_at);