-- SPDX-License-Identifier: Apache-2.0
-- Schema version 24: why the last run of a scheduled prompt failed, and when. A claim clears
-- both, so a row shows the error of its latest finished run only.

ALTER TABLE scheduled_prompts ADD COLUMN last_error TEXT;
ALTER TABLE scheduled_prompts ADD COLUMN last_error_at INTEGER;
