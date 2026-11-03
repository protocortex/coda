-- SPDX-License-Identifier: Apache-2.0
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
