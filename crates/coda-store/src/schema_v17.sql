-- SPDX-License-Identifier: Apache-2.0
-- Schema version 17: a session can work in a worktree that coda made for it.
-- A row with a `worktree_path` is the record that coda owns that folder and the
-- branch named here; nothing else is ever removed as a coda worktree.

ALTER TABLE sessions ADD COLUMN worktree_path TEXT;
ALTER TABLE sessions ADD COLUMN branch TEXT;
ALTER TABLE sessions ADD COLUMN worktree_repo TEXT;
