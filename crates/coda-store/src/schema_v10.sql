-- SPDX-License-Identifier: Apache-2.0
-- Schema version 10: a permission request and its decision name the sub-agent
-- that asked. Null is the session's own call, and what rows from before this
-- step are read as.

ALTER TABLE pending_permissions ADD COLUMN agent TEXT;
ALTER TABLE permission_decisions ADD COLUMN agent TEXT;
