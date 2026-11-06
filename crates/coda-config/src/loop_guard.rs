// SPDX-License-Identifier: Apache-2.0

//! The `[loop_guard]` table: when a turn that repeats itself is warned and stopped.

use serde::Deserialize;

/// Key of the table in `config.toml`.
pub const LOOP_GUARD_KEY: &str = "loop_guard";
/// Key of the warning threshold inside `[loop_guard]`.
pub const WARN_AFTER_KEY: &str = "warn_after";
/// Key of the stop threshold inside `[loop_guard]`.
pub const STOP_AFTER_KEY: &str = "stop_after";
/// Repeats of one call, with the same result, that earn the model a warning.
pub const DEFAULT_WARN_AFTER: u32 = 3;
/// Repeats that end the turn.
pub const DEFAULT_STOP_AFTER: u32 = 4;
/// Fewest repeats a threshold may name: one call alone is not a repeat.
pub const MIN_WARN_AFTER: u32 = 2;

/// The `[loop_guard]` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LoopGuardConfig {
    /// Times a call, or a pair of calls taking turns, must repeat with the
    /// same results before the model is warned. At least 2.
    pub warn_after: u32,
    /// Times it must repeat before the turn ends. Must be above `warn_after`.
    pub stop_after: u32,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            warn_after: DEFAULT_WARN_AFTER,
            stop_after: DEFAULT_STOP_AFTER,
        }
    }
}
