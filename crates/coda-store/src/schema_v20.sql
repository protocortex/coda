-- SPDX-License-Identifier: Apache-2.0
-- Schema version 20: cache token counts per usage record. Rows from before this version
-- keep zero, since nothing recorded their cache use.

ALTER TABLE usage ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_read_tokens >= 0);
ALTER TABLE usage ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_write_tokens >= 0);

-- Lets the newest events of one kind, such as plan limit reports, be read without a scan.
CREATE INDEX events_by_kind ON events (kind, id);
