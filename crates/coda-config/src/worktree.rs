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
}
