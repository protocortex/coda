-- SPDX-License-Identifier: AGPL-3.0-only
-- Schema version 9: repositories whose project agent definitions the user
-- trusts, with a digest of the definition files that were trusted.

CREATE TABLE project_trust (
    repo_path  TEXT PRIMARY KEY,
    digest     TEXT NOT NULL,
    trusted_at INTEGER NOT NULL
) STRICT;
