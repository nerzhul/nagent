-- 0009_user_memory_enabled.up.sql — per-user opt-in for the long-term
-- memory subsystem (plan 1791267136806).
--
-- Reversed by `0009_user_memory_enabled.down.sql`; keep the two
-- siblings in sync (sqlx 0.8.6 pairs them via the
-- `<version>_<name>.up.sql` / `<version>_<name>.down.sql` filename
-- convention).
--
-- The migration plan
-- (`.kilo/plans/1791267136806-memory-agent-plan.md` §1.6) locks
-- the default to `false`: the LLM-driven `memory_store` tool is
-- registered regardless of the flag, yet the system-prompt
-- paragraph and the auto-injection block branch on it. A user who
-- never opens Settings → Memory → never has anything injected and the
-- LLM is told to decline `memory_store` calls.
--
-- Stored as `INTEGER` (matches the existing
-- `share_location_enabled` / `share_timezone_enabled` shape; the
-- sqlite/per-domain repositories decode with `row_to_bool` rather
-- than relying on a native boolean).

ALTER TABLE user_preferences
  ADD COLUMN memory_enabled INTEGER NOT NULL DEFAULT 0;