-- SPDX-License-Identifier: AGPL-3.0-only
-- Schema version 6: incremental indexing state and fuller symbol spans.
--
-- `index_files` remembers the content hash and time each file was indexed, so
-- an unchanged file is skipped and a deleted one can be found and cleared.

ALTER TABLE symbols ADD COLUMN end_line INTEGER NOT NULL DEFAULT 0 CHECK (end_line >= 0);
ALTER TABLE symbols ADD COLUMN parent TEXT;

CREATE TABLE index_files (
    repo_path TEXT NOT NULL,
    file_path TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    indexed_at INTEGER NOT NULL,
    PRIMARY KEY (repo_path, file_path)
) STRICT, WITHOUT ROWID;

-- Files indexed before this version have no hash; the empty one never matches,
-- so the next update re-reads them.
INSERT INTO index_files (repo_path, file_path, content_hash, indexed_at)
SELECT DISTINCT repo_path, file_path, '', 0 FROM symbols;
