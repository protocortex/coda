// SPDX-License-Identifier: Apache-2.0

//! The `[worktree]` table: where coda puts the git worktrees it makes for
//! sessions and what it runs in a new one. Both come from the user's
//! `config.toml` only, so a project file cannot choose a program to run.

use std::path::PathBuf;

use serde::Deserialize;

/// Key of the table in `config.toml`.
pub const WORKTREE_KEY: &str = "worktree";
/// Key of the managed folder inside `[worktree]`.
pub const DIR_KEY: &str = "dir";
/// Key of the setup command inside `[worktree]`.
pub const SETUP_KEY: &str = "setup";

/// Idle days before a worktree can be removed, when `idle_days` is unset.
pub const DEFAULT_IDLE_DAYS: u32 = 14;
/// Key of the cleanup table inside `[worktree]`.
pub const CLEANUP_KEY: &str = "cleanup";
/// Key of the idle period inside `[worktree.cleanup]`.
pub const IDLE_DAYS_KEY: &str = "idle_days";
/// Key of the clean command inside `[worktree.cleanup]`.
pub const CLEAN_COMMAND_KEY: &str = "clean_command";

/// The `[worktree.cleanup]` section: when coda may remove the worktrees it made.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CleanupConfig {
    /// Whether `coda worktree clean --apply` may remove anything. Off by
    /// default; the report of what would go is always available.
    pub enabled: bool,
    /// A worktree whose session has been idle this many days can be removed.
    /// Must not be zero.
    pub idle_days: u32,
    /// The program and its arguments to run in a worktree before it is removed,
    /// for example `["cargo", "clean"]`. There is no shell. Empty runs nothing.
    pub clean_command: Vec<String>,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            idle_days: DEFAULT_IDLE_DAYS,
            clean_command: Vec::new(),
        }
    }
}

/// The `[worktree]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorktreeConfig {
    /// The folder that holds every worktree coda makes. An absolute path;
    /// `worktrees` inside the data folder when unset.
    pub dir: Option<PathBuf>,
    /// The program and its arguments to run in a new worktree before the
    /// session starts. There is no shell. Empty runs nothing.
    pub setup: Vec<String>,
    /// When the worktrees may be removed again.
    pub cleanup: CleanupConfig,
}
