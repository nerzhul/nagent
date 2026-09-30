-- 0002_credentials.up.sql — per-user credentials vault + audit column.
--
-- Reversed by `0002_credentials.down.sql`; keep the two siblings in
-- sync (sqlx 0.8.6 pairs them via the `<version>_<name>.up.sql` /
-- `<version>_<name>.down.sql` filename convention).
--
-- This migration extends the auth schema with the per-user
-- credentials framework: one row per (user, service, field) holds
-- the AES-256-GCM nonce and ciphertext for a single plaintext
-- field. The encryption key itself is server-side and never
-- persisted in this table; see `[auth.credentials].key` and
-- `crates/stt-server/src/credentials/key.rs`.
--
-- Also adds `auth_events.target_service` so audit log lines can be
-- correlated with the integration that triggered them. Backfilled
-- as NULL on rows that pre-date the per-user credentials framework.

ALTER TABLE auth_events ADD COLUMN target_service TEXT;

CREATE TABLE user_credentials (
    id              TEXT PRIMARY KEY,
    user_id         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    service_id      TEXT NOT NULL,
    field_key       TEXT NOT NULL,
    nonce           BLOB NOT NULL,
    ciphertext      BLOB NOT NULL,
    created_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at      TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (user_id, service_id, field_key)
);

CREATE INDEX user_credentials_user_service_idx
    ON user_credentials(user_id, service_id);
