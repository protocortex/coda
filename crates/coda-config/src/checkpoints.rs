// SPDX-License-Identifier: Apache-2.0

//! The `[checkpoints.prune]` table: when coda removes the git refs that keep
//! old checkpoints alive.

use serde::Deserialize;

/// Key of the table in `config.toml`.
pub const CHECKPOINTS_KEY: &str = "checkpoints";
/// Key of the pruning table inside `[checkpoints]`.
pub const PRUNE_KEY: &str = "prune";
/// Key of the age limit of turn checkpoints.
pub const KEEP_DAYS_KEY: &str = "keep_days";
/// Key of the count limit of turn checkpoints.
pub const KEEP_TURNS_KEY: &str = "keep_turns";
/// Key of the age limit of the safety saves a rewind makes.
pub const REWIND_DAYS_KEY: &str = "rewind_days";
/// Days a turn checkpoint is kept when `keep_days` is unset.
pub const DEFAULT_KEEP_DAYS: u32 = 30;
/// Newest turn checkpoints a session keeps when `keep_turns` is unset.
pub const DEFAULT_KEEP_TURNS: u32 = 50;
/// Days a rewind safety save is kept when `rewind_days` is unset.
pub const DEFAULT_REWIND_DAYS: u32 = 7;

/// The `[checkpoints.prune]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PruneConfig {
    /// Whether `coda checkpoints prune --apply` may remove anything. Off by
    /// default; the report of what would go is always available.
    pub enabled: bool,
    /// A turn checkpoint older than this many days can be removed.
    pub keep_days: u32,
    /// Only the newest this many turn checkpoints of a session are kept.
    pub keep_turns: u32,
    /// A rewind safety save (`pre-rewind-*`) older than this many days can be removed.
    pub rewind_days: u32,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            keep_days: DEFAULT_KEEP_DAYS,
            keep_turns: DEFAULT_KEEP_TURNS,
            rewind_days: DEFAULT_REWIND_DAYS,
        }
    }
}

/// The `[checkpoints]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CheckpointsConfig {
    /// When old checkpoints are removed.
    pub prune: PruneConfig,
}
