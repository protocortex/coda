// SPDX-License-Identifier: Apache-2.0

//! The `[checks]` table: a command coda runs after the model changes a file,
//! chosen by the file's extension. Checks come from the user's `config.toml`
//! only, like hooks, so a project file cannot add one.

use std::time::Duration;

use serde::Deserialize;

/// How long a check may run when its entry sets no `timeout_ms`.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// One `[checks.<extension>]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckConfig {
    /// The program and its arguments. There is no shell. The program is an
    /// absolute path or a bare name found in the absolute entries of `PATH`.
    pub command: Vec<String>,
    /// Longest run time in milliseconds; [`DEFAULT_TIMEOUT_MS`] when unset.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl CheckConfig {
    /// How long the check may run before it is reported as timed out.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))
    }
}
