-- SPDX-License-Identifier: AGPL-3.0-only
-- Snapshot of schema version 4, used to test forward migration.

-- Schema version 1.
--
-- Privacy rule: `usage` and `event_log` are content-free. They hold integer
-- ids, timestamps, and TEXT columns constrained to an allowlist or a short
-- length, never free text. `events` and `originals` hold content.

CREATE TABLE sessions (
    id INTEGER PRIMARY KEY,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE turns (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    started_at INTEGER NOT NULL,
    ended_at INTEGER
) STRICT;

CREATE INDEX turns_by_session ON turns (session_id);

CREATE TABLE events (
    id INTEGER PRIMARY KEY,
    turn_id INTEGER NOT NULL REFERENCES turns (id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    body TEXT NOT NULL,
    at INTEGER NOT NULL,
    UNIQUE (turn_id, seq)
) STRICT;

CREATE INDEX events_by_turn ON events (turn_id);

CREATE TABLE usage (
    id INTEGER PRIMARY KEY,
    turn_id INTEGER NOT NULL REFERENCES turns (id) ON DELETE CASCADE,
    input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
    at INTEGER NOT NULL
) STRICT;

CREATE INDEX usage_by_turn ON usage (turn_id);

CREATE TABLE event_log (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN (
        'session_started', 'session_ended', 'turn_started', 'turn_ended',
        'tool_called', 'permission_requested', 'permission_decided',
        'index_updated', 'error'
    )),
    at INTEGER NOT NULL,
    session_id INTEGER,
    turn_id INTEGER,
    scope TEXT CHECK (scope IN ('session', 'repo', 'global')),
    producer_id INTEGER
) STRICT;

CREATE INDEX event_log_by_at ON event_log (at);

CREATE TABLE originals (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    content BLOB NOT NULL,
    at INTEGER NOT NULL
) STRICT;

CREATE INDEX originals_by_session ON originals (session_id);

CREATE TABLE symbols (
    id INTEGER PRIMARY KEY,
    repo_path TEXT NOT NULL,
    file_path TEXT NOT NULL,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    line INTEGER NOT NULL CHECK (line >= 0)
) STRICT;

CREATE INDEX symbols_by_file ON symbols (repo_path, file_path);

CREATE VIRTUAL TABLE symbols_fts USING fts5 (
    name,
    content = 'symbols',
    content_rowid = 'id'
);

CREATE TRIGGER symbols_fts_insert AFTER INSERT ON symbols BEGIN
    INSERT INTO symbols_fts (rowid, name) VALUES (new.id, new.name);
END;

CREATE TRIGGER symbols_fts_delete AFTER DELETE ON symbols BEGIN
    INSERT INTO symbols_fts (symbols_fts, rowid, name)
    VALUES ('delete', old.id, old.name);
END;

CREATE TRIGGER symbols_fts_update AFTER UPDATE ON symbols BEGIN
    INSERT INTO symbols_fts (symbols_fts, rowid, name)
    VALUES ('delete', old.id, old.name);
    INSERT INTO symbols_fts (rowid, name) VALUES (new.id, new.name);
END;

CREATE TABLE pending_permissions (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    repo_path TEXT NOT NULL,
    command TEXT NOT NULL,
    requested_at INTEGER NOT NULL
) STRICT;

CREATE INDEX pending_permissions_by_session ON pending_permissions (session_id);

CREATE TABLE permission_decisions (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    repo_path TEXT NOT NULL,
    command TEXT NOT NULL,
    decision TEXT NOT NULL CHECK (decision IN ('allow', 'deny')),
    decided_at INTEGER NOT NULL
) STRICT;

CREATE INDEX permission_decisions_by_session ON permission_decisions (session_id);

CREATE TABLE permission_grants (
    id INTEGER PRIMARY KEY,
    repo_path TEXT NOT NULL,
    command_pattern TEXT NOT NULL,
    granted_at INTEGER NOT NULL,
    UNIQUE (repo_path, command_pattern)
) STRICT;

-- Schema version 2: session details and resolved permission requests.

ALTER TABLE sessions ADD COLUMN backend TEXT NOT NULL DEFAULT 'faux';
ALTER TABLE sessions ADD COLUMN model TEXT;
ALTER TABLE sessions ADD COLUMN cwd TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'default';

ALTER TABLE pending_permissions ADD COLUMN resolved_at INTEGER;


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

-- Schema version 4: a pending permission request names its tool, and events are indexed by time.

ALTER TABLE pending_permissions ADD COLUMN tool TEXT NOT NULL DEFAULT '';

-- Lets the session list read each session's last event time without a scan.
CREATE INDEX events_by_turn_and_time ON events (turn_id, at);
