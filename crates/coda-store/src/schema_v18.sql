-- SPDX-License-Identifier: Apache-2.0
-- Schema version 18: follow-up messages a client sent while a turn was running.
-- They wait here, oldest first, until the turn ends and the daemon starts the
-- next one. They survive a restart, so a queue is never lost.

CREATE TABLE queued_messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    text       TEXT NOT NULL,
    max_turns  INTEGER,
    token_cap  INTEGER,
    created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX queued_messages_by_session ON queued_messages (session_id, id);
