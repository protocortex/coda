-- SPDX-License-Identifier: AGPL-3.0-only
-- Schema version 8: a key/value table for facts about the database itself.
--
-- `last_release` holds the semver of the binary that last opened it, so a
-- later open runs only the migration steps introduced after that release.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
