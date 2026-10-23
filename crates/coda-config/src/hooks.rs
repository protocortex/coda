// SPDX-License-Identifier: AGPL-3.0-only

//! The `[[hooks]]` table: commands coda runs at session start and around tool
//! calls. Hooks come from the user's `config.toml` only. No definition file and
//! no project file can add one.

use std::fmt;
use std::time::Duration;

use serde::Deserialize;

/// How long a hook may run when its entry sets no `timeout_ms`.
pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// The longest `timeout_ms` a hook may ask for.
pub const MAX_TIMEOUT_MS: u64 = 300_000;

/// When a hook runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before the first turn of a session's conversation.
    SessionStart,
    /// After a tool call is allowed and before the tool runs.
    PreTool,
    /// After a tool ran.
    PostTool,
}

impl HookEvent {
    /// The name used in `config.toml` and in a hook's payload.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "session_start",
            Self::PreTool => "pre_tool",
            Self::PostTool => "post_tool",
        }
    }
}

impl fmt::Display for HookEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `[[hooks]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// When the hook runs.
    pub event: HookEvent,
    /// The program and its arguments. There is no shell: a string here is an error.
    /// The program is an absolute path or a bare name found in the absolute
    /// entries of the hook's `PATH`; a relative path is an error.
    pub command: Vec<String>,
    /// Whether a failure stops the action. A hook that is not blocking only logs.
    #[serde(default)]
    pub blocking: bool,
    /// Longest run time in milliseconds; [`DEFAULT_TIMEOUT_MS`] when unset.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

impl HookConfig {
    /// How long the hook may run before it counts as failed.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS))
    }
}
