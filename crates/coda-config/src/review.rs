// SPDX-License-Identifier: Apache-2.0

//! The `[review]` table: limits for `coda review`.

use serde::Deserialize;

/// Key of the table in `config.toml`.
pub const REVIEW_KEY: &str = "review";
/// Key of the finding cap inside `[review]`.
pub const MAX_FINDINGS_KEY: &str = "max_findings";
/// Findings kept per run when the file does not say.
pub const DEFAULT_MAX_FINDINGS: usize = 5;

/// The `[review]` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ReviewConfig {
    /// Most findings one review run reports. Must not be zero.
    pub max_findings: usize,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            max_findings: DEFAULT_MAX_FINDINGS,
        }
    }
}
