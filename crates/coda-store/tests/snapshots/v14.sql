-- SPDX-License-Identifier: Apache-2.0
-- Snapshot of schema version 14, used to test forward migration.

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

-- Schema version 4: a pending permission request names its tool, and events are indexed by time.

ALTER TABLE pending_permissions ADD COLUMN tool TEXT NOT NULL DEFAULT '';

-- Lets the session list read each session's last event time without a scan.
CREATE INDEX events_by_turn_and_time ON events (turn_id, at);

-- Schema version 5: bytes the token economy kept out of the context, per session.
--
-- Content-free like `usage`: integer sizes and a timestamp only.

CREATE TABLE savings (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    original_bytes INTEGER NOT NULL CHECK (original_bytes >= 0),
    compressed_bytes INTEGER NOT NULL CHECK (compressed_bytes >= 0 AND compressed_bytes <= original_bytes),
    at INTEGER NOT NULL
) STRICT;

CREATE INDEX savings_by_session ON savings (session_id);

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

-- Schema version 7: close the turns that began before turns were ended.
--
-- Nothing set `turns.ended_at` before this version, so every old turn looks
-- unfinished and startup recovery would mark each one interrupted. A turn
-- from before this version is over, so it ends at its latest event, or at its
-- start when it has none.

UPDATE turns
SET ended_at = MAX(
    started_at,
    COALESCE((SELECT MAX(at) FROM events WHERE events.turn_id = turns.id), started_at)
)
WHERE ended_at IS NULL;

-- Schema version 8: a key/value table for facts about the database itself.
--
-- `last_release` holds the semver of the binary that last opened it, so a
-- later open runs only the migration steps introduced after that release.

CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

-- Schema version 9: repositories whose project agent definitions the user
-- trusts, with a digest of the definition files that were trusted.

CREATE TABLE project_trust (
    repo_path  TEXT PRIMARY KEY,
    digest     TEXT NOT NULL,
    trusted_at INTEGER NOT NULL
) STRICT;

-- Schema version 10: a permission request and its decision name the sub-agent
-- that asked. Null is the session's own call, and what rows from before this
-- step are read as.

ALTER TABLE pending_permissions ADD COLUMN agent TEXT;
ALTER TABLE permission_decisions ADD COLUMN agent TEXT;

-- Schema version 11: the backend's own conversation id per session, so the
-- first turn after a daemon restart resumes it. `status` is `active`, or
-- `error` once a resume of `native_ref` failed and it must not be tried again.

CREATE TABLE backend_threads (
    session_id INTEGER NOT NULL REFERENCES sessions (id) ON DELETE CASCADE,
    backend    TEXT NOT NULL,
    native_ref TEXT NOT NULL,
    first_turn TEXT NOT NULL,
    last_turn  TEXT NOT NULL,
    status     TEXT NOT NULL CHECK (status IN ('active', 'error')),
    PRIMARY KEY (session_id, backend)
) STRICT;

-- Schema version 12: the turn columns of `backend_threads` were written and
-- never read, so they go. `status` stays: it is always `active`
-- now that nothing marks a stored id as failed.

ALTER TABLE backend_threads DROP COLUMN first_turn;
ALTER TABLE backend_threads DROP COLUMN last_turn;

-- Schema version 13: the checkpoint refs of a turn. `checkpoint_before` is
-- set on the first turn of a session only and names the snapshot taken before
-- it; `checkpoint_after` names the snapshot taken when the turn ended. Both
-- are NULL when the folder is not a git repository or a snapshot failed.

ALTER TABLE turns ADD COLUMN checkpoint_before TEXT;
ALTER TABLE turns ADD COLUMN checkpoint_after TEXT;

-- Schema version 14: a permission request keeps the reason the backend gave
-- for asking, so a client that resumes shows it again. Null is no reason, and
-- what rows from before this step are read as.

ALTER TABLE pending_permissions ADD COLUMN reason TEXT;
