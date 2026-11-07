-- SPDX-License-Identifier: Apache-2.0
-- Schema version 19: what a command with a client request id answered. A repeat of the
-- request returns the stored answer instead of doing the work again. A turn start writes
-- its row in the same transaction as the turn.

CREATE TABLE command_receipts (
    request_id  TEXT PRIMARY KEY,
    session_id  INTEGER REFERENCES sessions (id) ON DELETE CASCADE,
    method      TEXT NOT NULL,
    result_json TEXT NOT NULL,
    at          INTEGER NOT NULL
) STRICT;

CREATE INDEX command_receipts_by_time ON command_receipts (at);
