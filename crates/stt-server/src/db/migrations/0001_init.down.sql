-- 0001_init.down.sql — reverses 0001_init.up.sql.
--
-- The schema has FK references `sessions.user_id` and `passkeys.user_id`
-- → `users.id`, so the rollback MUST drop the dependents before
-- dropping `users`. `auth_events.user_id` is a nullable loose reference
-- (no declared FK) and `pending_oidc_states` is fully standalone. All
-- `DROP` statements use `IF EXISTS` so the down migration is
-- idempotent against partial rollbacks and is portable across sqlite +
-- postgres (`DROP INDEX IF EXISTS` + `DROP TABLE IF EXISTS` are
-- supported by both).
--
-- The sqlite minimum version for `DROP TABLE` on a table that has
-- generated column dependencies is >= 3.35; that floor is the same as
-- the one required by `0002_credentials.down.sql` (ALTER TABLE DROP
-- COLUMN), documented in README §"Storage engine".

DROP INDEX IF EXISTS pending_oidc_states_expires_at_idx;
DROP INDEX IF EXISTS auth_events_occurred_at_idx;
DROP INDEX IF EXISTS auth_events_user_id_idx;
DROP INDEX IF EXISTS sessions_expires_at_idx;
DROP INDEX IF EXISTS sessions_user_id_idx;
DROP INDEX IF EXISTS passkeys_user_id_idx;
DROP INDEX IF EXISTS users_provider_idx;

DROP TABLE IF EXISTS pending_oidc_states;
DROP TABLE IF EXISTS auth_events;
DROP TABLE IF EXISTS sessions;
DROP TABLE IF EXISTS passkeys;
DROP TABLE IF EXISTS users;
