-- SPDX-License-Identifier: Apache-2.0
-- Schema version 22: scheduled prompts. One row per schedule. The columns of its kind are set
-- and the others stay null. A run is claimed by moving `next_run_at` forward and setting
-- `state` to 'running' in one transaction, so a crash leaves a row that start-up can find.

CREATE TABLE scheduled_prompts (
    id              INTEGER PRIMARY KEY,
    kind            TEXT NOT NULL CHECK (kind IN ('once', 'interval', 'daily')),
    at              INTEGER,
    every_secs      INTEGER CHECK (every_secs IS NULL OR every_secs >= 60),
    minute_of_day   INTEGER CHECK (minute_of_day IS NULL OR (minute_of_day >= 0 AND minute_of_day < 1440)),
    weekdays        INTEGER NOT NULL DEFAULT 0 CHECK (weekdays >= 0 AND weekdays < 128),
    target_session  INTEGER REFERENCES sessions (id) ON DELETE CASCADE,
    backend         TEXT,
    model           TEXT,
    cwd             TEXT,
    mode            TEXT,
    text            TEXT NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('idle', 'running', 'done')),
    next_run_at     INTEGER,
    last_run_at     INTEGER,
    runs            INTEGER NOT NULL DEFAULT 0 CHECK (runs >= 0),
    created_at      INTEGER NOT NULL
);

CREATE INDEX scheduled_prompts_due ON scheduled_prompts (state, next_run_at);
