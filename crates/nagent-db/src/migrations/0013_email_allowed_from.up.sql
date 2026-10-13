-- 0013_email_allowed_from.up.sql — per-user `from:` allow-list for
-- the email SMTP probe (plan 1791566269874, §6).
--
-- Reversed by `0013_email_allowed_from.down.sql`; keep the two
-- siblings in sync (sqlx 0.8.6 pairs them via the
-- `<version>_<name>.up.sql` / `<version>_<name>.down.sql` filename
-- convention).
--
-- One row per `(user, from_address)` the SMTP probe accepted on
-- `MAIL FROM:<addr>`. The probe runs at save time and on operator
-- request (a 7-day re-probe cap is enforced in Rust so a server
-- policy change propagates without an operator-driven sweep).
--
-- Column shape:
--   user_id    TEXT NOT NULL → users.id (CASCADE delete)
--   from_addr  TEXT NOT NULL — verified envelope sender
--   probed_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
--
-- The `from_addr` is the SMTP probe verdict, not the LLM's
-- argument: the `email_send` agent always reads the saved
-- `from_address` from the per-user vault and rejects any value
-- the LLM tries to inject. The SMTP probe is the only path that
-- writes to this table.
--
-- Portability notes (mirrors `0002_credentials.up.sql`):
--   * `TEXT PRIMARY KEY` for UUID columns works on both sqlite and
--   postgres (stored as TEXT, not the native `uuid` type).
--   * `CURRENT_TIMESTAMP` returns the same RFC 3339-like string
--   on both engines so the Rust parser does not branch.

CREATE TABLE email_allowed_from (
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    from_addr  TEXT NOT NULL,
    probed_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (user_id, from_addr)
);

CREATE INDEX email_allowed_from_user_idx
    ON email_allowed_from (user_id);
