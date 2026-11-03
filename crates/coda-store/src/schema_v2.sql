-- SPDX-License-Identifier: Apache-2.0
-- Schema version 2: session details and resolved permission requests.

ALTER TABLE sessions ADD COLUMN backend TEXT NOT NULL DEFAULT 'faux';
ALTER TABLE sessions ADD COLUMN model TEXT;
ALTER TABLE sessions ADD COLUMN cwd TEXT NOT NULL DEFAULT '';
ALTER TABLE sessions ADD COLUMN mode TEXT NOT NULL DEFAULT 'default';

ALTER TABLE pending_permissions ADD COLUMN resolved_at INTEGER;
