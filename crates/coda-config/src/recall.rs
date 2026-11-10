// SPDX-License-Identifier: Apache-2.0

//! The `[recall]` table: whether earlier sessions are indexed for the `recall`
//! tool. It is off by default, so no message text is copied into the search
//! index until the user switches it on in their `config.toml`.

use std::path::Path;

use serde::Deserialize;

/// Key of the table in `config.toml`.
pub const RECALL_KEY: &str = "recall";
/// Key of the on switch inside `[recall]`.
pub const ENABLED_KEY: &str = "enabled";
/// Key of the repository list inside `[recall]`.
pub const DELEGATED_REPOS_KEY: &str = "delegated_repos";

/// The `[recall]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecallConfig {
    /// Whether messages and tool summaries are indexed and the `recall` tool is
    /// offered to model-API sessions. Off, the index is emptied at daemon start.
    pub enabled: bool,
    /// Repositories, as absolute folders, whose Claude Code and Codex sessions
    /// the tool may also search. Without an entry only sessions on a model API
    /// are searched.
    pub delegated_repos: Vec<String>,
}

/// Whether `entry` is an absolute folder path.
#[must_use]
pub fn is_repo_path(entry: &str) -> bool {
    Path::new(entry).is_absolute()
}
