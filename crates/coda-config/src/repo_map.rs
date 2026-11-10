// SPDX-License-Identifier: Apache-2.0

//! The `[repo_map]` table: the outline of the repository that a model-API
//! session gets as its first context message.

use serde::Deserialize;

/// Key of the table in `config.toml`.
pub const REPO_MAP_KEY: &str = "repo_map";
/// Key of the on switch inside `[repo_map]`.
pub const ENABLED_KEY: &str = "enabled";
/// Key of the size limit inside `[repo_map]`.
pub const TOKEN_SHARE_KEY: &str = "token_share_percent";

/// Percent of the model's context window the map may use when the key is unset.
pub const DEFAULT_TOKEN_SHARE_PERCENT: u32 = 2;
/// The most `token_share_percent` may be.
pub const MAX_TOKEN_SHARE_PERCENT: u32 = 20;

/// The `[repo_map]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoMapConfig {
    /// Whether a session starts with a map of the repository.
    pub enabled: bool,
    /// The most of the model's context window the map may take, in percent.
    pub token_share_percent: u32,
}

impl Default for RepoMapConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            token_share_percent: DEFAULT_TOKEN_SHARE_PERCENT,
        }
    }
}
