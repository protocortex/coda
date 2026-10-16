-- SPDX-License-Identifier: AGPL-3.0-only
-- Schema version 3: a grant is keyed by repo, tool, cwd and exact pattern.

-- The grants API shipped in this same release, so the table holds no production data to keep.
DROP TABLE permission_grants;

CREATE TABLE permission_grants (
    id INTEGER PRIMARY KEY,
    repo_path TEXT NOT NULL,
    tool TEXT NOT NULL,
    cwd TEXT NOT NULL,
    command_pattern TEXT NOT NULL,
    granted_at INTEGER NOT NULL,
    UNIQUE (repo_path, tool, cwd, command_pattern)
) STRICT;
