-- 0011_documents_pages.down.sql — undo `0011_documents_pages.up.sql`.
--
-- Drops the new column. The on-disk `<uuid>/pages/` directories
-- (and any `page-NNNN.bin` / `meta.bin` files inside them) are
-- NOT removed by `down` — the operator is expected to clear the
-- documents cache directory manually, mirroring the
-- `0003_uploaded_documents.down.sql` rationale (decoupling the
-- schema rollback from a bulk file delete).
ALTER TABLE uploaded_documents DROP COLUMN pages_dir;