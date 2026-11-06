-- SPDX-License-Identifier: Apache-2.0
-- Schema version 12: the turn columns of `backend_threads` were written and
-- never read, so they go. `status` stays: it is always `active`
-- now that nothing marks a stored id as failed.

ALTER TABLE backend_threads DROP COLUMN first_turn;
ALTER TABLE backend_threads DROP COLUMN last_turn;
