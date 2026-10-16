-- SPDX-License-Identifier: AGPL-3.0-only
-- Schema version 2: session details and resolved permission requests.

ALTER TABLE sessions ADD COLUMN backend TEXT NOT NULL DEFAULT 'faux';
ALTER TABLE sessions ADD COLUMN model TEXT;
ALTER TABLE sessions ADD COLUMN cwd TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'default';

ALTER TABLE pending_permissions ADD COLUMN resolved_at INTEGER;
