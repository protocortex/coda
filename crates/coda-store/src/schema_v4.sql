-- SPDX-License-Identifier: AGPL-3.0-only
-- Schema version 4: a pending permission request names its tool, and events are indexed by time.

ALTER TABLE pending_permissions ADD COLUMN tool TEXT NOT NULL DEFAULT '';

-- Lets the session list read each session's last event time without a scan.
CREATE INDEX events_by_turn_and_time ON events (turn_id, at);
