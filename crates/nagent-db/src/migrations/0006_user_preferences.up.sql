-- 0006_user_preferences.up.sql — per-user UI preferences.
--
-- Several Discussion-mode opt-ins ("share my location with the LLM",
-- "forward the browser timezone to the LLM") were previously stored
-- in `localStorage`, keyed under `nagent.chat.*`. That meant the
-- toggle did not survive a browser switch, a private-browsing
-- session, or a profile reset — and worse, a user who opted in on
-- one device kept getting the location block on every other device
-- they happened to share the laptop with. This migration hoists
-- those flags into the auth DB so the same user gets the same
-- behaviour regardless of where they log in.
--
-- Storage layout (one row per user):
--   user_id                    TEXT PRIMARY KEY REFERENCES users(id)
--                              ON DELETE CASCADE — preferences are
--                              strictly scoped to a single account;
--                              deleting the account wipes its row.
--   share_location_enabled     INTEGER NOT NULL DEFAULT 0 — mirrors
--                              the previous LOCATION_ENABLED_KEY
--                              localStorage flag.
--   share_timezone_enabled     INTEGER NOT NULL DEFAULT 0 — mirrors
--                              the previous TIMEZONE_ENABLED_KEY
--                              localStorage flag.
--   updated_at                 TEXT NOT NULL DEFAULT
--                              CURRENT_TIMESTAMP — last write time,
--                              surfaced by the GET handler so the UI
--                              can tell the user when the choice was
--                              last changed (useful for audit + to
--                              detect a stale UI cache).
--
-- The columns are 0/1 integers (not booleans) so the schema works
-- identically on sqlite and postgres — sqlite stores them as INTEGER
-- (truthy via `i != 0`), postgres as SMALLINT or BOOLEAN (truthy via
-- the standard `bool` cast). The Rust side reads them as `bool` via a
-- `i64 -> bool` cast (`!= 0`), which both engines agree on.
--
-- No additional indexes needed: lookups are always by `user_id`, the
-- primary key.

CREATE TABLE user_preferences (
    user_id                    TEXT PRIMARY KEY
                               REFERENCES users(id) ON DELETE CASCADE,
    share_location_enabled     INTEGER NOT NULL DEFAULT 0,
    share_timezone_enabled     INTEGER NOT NULL DEFAULT 0,
    updated_at                 TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);