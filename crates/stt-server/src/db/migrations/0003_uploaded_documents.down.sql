-- 0003_uploaded_documents.down.sql — undo `0003_uploaded_documents.up.sql`.
--
-- Drops the table + its indexes. Files under `documents.cache_dir`
-- are NOT removed by `down` — the operator is expected to run
-- `stt-server documents purge --older-than 0s` (or wipe the cache
-- dir manually) to clean the on-disk side. Doing both in the
-- migration would force operators who only want to roll back the
-- schema to also confirm a bulk file delete.
DROP INDEX IF EXISTS uploaded_documents_session_idx;
DROP INDEX IF EXISTS uploaded_documents_created_at_idx;
DROP TABLE IF EXISTS uploaded_documents;