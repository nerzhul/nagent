-- 0012_chat_messages.down.sql — undo `0012_chat_messages.up.sql`.
--
-- Drops the table (the two indexes drop with it). The
-- localStorage copy of every message lives under
-- `nagent.chat.session.<id>` so a downgrade is non-destructive
-- for the user: the browser keeps rendering from localStorage and
-- the migration back to the server is rerun on the next page
-- load after the upgrade is restored.
DROP TABLE IF EXISTS chat_messages;
