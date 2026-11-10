-- SPDX-License-Identifier: Apache-2.0
-- Schema version 25: a full-text index over the text of earlier sessions, for the `recall`
-- tool. A row copies one message or one short tool summary the store already holds, so the
-- index goes away with its session, its turn or its event, and a deleted original takes the
-- summary made from it along. It is filled only while recall is switched on.

CREATE TABLE recall_entries (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    turn_id INTEGER NOT NULL REFERENCES turns (id) ON DELETE CASCADE,
    event_id INTEGER NOT NULL UNIQUE REFERENCES events (id) ON DELETE CASCADE,
    original_id INTEGER,
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    at INTEGER NOT NULL
) STRICT;

CREATE INDEX recall_entries_by_session ON recall_entries (session_id);
CREATE INDEX recall_entries_by_original ON recall_entries (original_id)
    WHERE original_id IS NOT NULL;

CREATE VIRTUAL TABLE recall_fts USING fts5 (
    body,
    content = 'recall_entries',
    content_rowid = 'id'
);

CREATE TRIGGER recall_fts_insert AFTER INSERT ON recall_entries BEGIN
    INSERT INTO recall_fts (rowid, body) VALUES (new.id, new.body);
END;

CREATE TRIGGER recall_fts_delete AFTER DELETE ON recall_entries BEGIN
    INSERT INTO recall_fts (recall_fts, rowid, body)
    VALUES ('delete', old.id, old.body);
END;
