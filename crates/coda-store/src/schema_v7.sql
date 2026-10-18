-- SPDX-License-Identifier: AGPL-3.0-only
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
