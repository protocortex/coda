-- SPDX-License-Identifier: Apache-2.0
-- Schema version 13: the checkpoint refs of a turn. `checkpoint_before` is
-- set on the first turn of a session only and names the snapshot taken before
-- it; `checkpoint_after` names the snapshot taken when the turn ended. Both
-- are NULL when the folder is not a git repository or a snapshot failed.

ALTER TABLE turns ADD COLUMN checkpoint_before TEXT;
ALTER TABLE turns ADD COLUMN checkpoint_after TEXT;
