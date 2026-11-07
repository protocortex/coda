-- SPDX-License-Identifier: Apache-2.0
-- Schema version 21: the pull requests a session opened, from its ship request or from a
-- `gh pr create` its agent ran. One row per session and address.

CREATE TABLE session_pull_requests (
    id          INTEGER PRIMARY KEY,
    session_id  INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    url         TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    UNIQUE (session_id, url)
);
