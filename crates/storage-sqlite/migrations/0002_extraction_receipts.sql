-- Receipts outlive the bounded activity log. Pruning logs must not restart extraction.
CREATE TABLE extraction_receipts (
 source_id TEXT PRIMARY KEY REFERENCES sources(id), processed_at INTEGER NOT NULL
);
INSERT OR IGNORE INTO extraction_receipts(source_id,processed_at)
 SELECT substr(kind,9), created_at FROM activities WHERE kind LIKE 'extract:%';
