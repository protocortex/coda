-- SPDX-License-Identifier: Apache-2.0
-- Schema version 15: a rewind marks the turns it undoes. They stay stored and
-- shown, but the conversation a backend is given leaves them out.

ALTER TABLE turns ADD COLUMN rolled_back_at INTEGER;
