-- SPDX-License-Identifier: Apache-2.0
-- Schema version 22: the session and turn a forked session came from. One row per fork.

CREATE TABLE session_forks (
    session_id         INTEGER PRIMARY KEY REFERENCES sessions (id) ON DELETE CASCADE,
    parent_session_id  INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    source_turn        INTEGER NOT NULL
);
