-- SPDX-License-Identifier: Apache-2.0
-- Schema version 16: what `coda review` remembers about a pull request. One row per
-- run records the head commit it reviewed; one row per finding records its fingerprint,
-- where it was, a hash of the code lines it pointed at, and whether it is still open,
-- was resolved by a later change, or was dismissed by the user.

CREATE TABLE review_runs (
    id          INTEGER PRIMARY KEY,
    pr_key      TEXT NOT NULL,
    head_sha    TEXT NOT NULL,
    full        INTEGER NOT NULL CHECK (full IN (0, 1)),
    created_at  INTEGER NOT NULL
);

CREATE INDEX review_runs_by_pr ON review_runs (pr_key, id);

CREATE TABLE review_findings (
    pr_key       TEXT NOT NULL,
    fingerprint  TEXT NOT NULL,
    file         TEXT NOT NULL,
    start_line   INTEGER NOT NULL,
    end_line     INTEGER NOT NULL,
    severity     TEXT NOT NULL,
    title        TEXT NOT NULL,
    explanation  TEXT NOT NULL,
    code_hash    TEXT NOT NULL,
    status       TEXT NOT NULL CHECK (status IN ('open', 'resolved', 'dismissed')),
    first_run    INTEGER NOT NULL REFERENCES review_runs (id),
    last_run     INTEGER NOT NULL REFERENCES review_runs (id),
    PRIMARY KEY (pr_key, fingerprint)
);
