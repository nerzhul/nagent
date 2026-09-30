-- 0006_user_preferences.down.sql — reverts the per-user preferences
-- table introduced by 0006_user_preferences.up.sql. Used by the
-- `migrate revert` CLI for symmetry only; running it in production
-- silently drops every saved toggle (every logged-in user will see
-- the location / timezone blocks stop being sent on the next
-- request).
DROP TABLE IF EXISTS user_preferences;