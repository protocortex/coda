-- SPDX-License-Identifier: Apache-2.0
-- Schema version 23: the session and turn a forked session came from. One row per fork.
-- `rolled_back` lists the turns the parent had rolled back when the fork was made, as
-- turn ids separated by commas, so a later rewind of the parent does not change the fork.

CREATE TABLE session_forks (
    session_id         INTEGER PRIMARY KEY REFERENCES sessions (id) ON DELETE CASCADE,
    parent_session_id  INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    source_turn        INTEGER NOT NULL,
    rolled_back        TEXT NOT NULL DEFAULT ''
);
