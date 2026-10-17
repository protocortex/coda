-- SPDX-License-Identifier: AGPL-3.0-only
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
