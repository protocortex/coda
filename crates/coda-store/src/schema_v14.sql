-- SPDX-License-Identifier: Apache-2.0
-- Schema version 14: a permission request keeps the reason the backend gave
-- for asking, so a client that resumes shows it again. Null is no reason, and
-- what rows from before this step are read as.

ALTER TABLE pending_permissions ADD COLUMN reason TEXT;
