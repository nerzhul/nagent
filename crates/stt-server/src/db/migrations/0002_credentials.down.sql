-- 0002_credentials.down.sql — reverses 0002_credentials.up.sql.
--
-- Drops the `user_credentials` table + its index BEFORE the
-- `ALTER TABLE … DROP COLUMN` because dropping the column first
-- would still leave the table referenced. SQLite requires
-- >= 3.35 for `ALTER TABLE … DROP COLUMN` (released March 2021);
-- the auth subsystem already needs sqlite 3.35+ for `DROP COLUMN`
-- in this file, and the build script relies on the same floor for
-- the WAL + foreign-keys PRAGMAs in `db_sqlite.rs`. Documented in
-- README §"Storage engine".

DROP INDEX IF EXISTS user_credentials_user_service_idx;
DROP TABLE IF EXISTS user_credentials;

ALTER TABLE auth_events DROP COLUMN target_service;
