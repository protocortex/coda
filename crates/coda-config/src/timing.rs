// SPDX-License-Identifier: AGPL-3.0-only

//! Injectable timeouts and backoffs. In `config.toml` each is an optional
//! integer number of milliseconds under `[timing]`, named `<field>_ms`.

use std::time::Duration;

use serde::Deserialize;

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_RETRY_BACKOFF_BASE: Duration = Duration::from_millis(500);
const DEFAULT_RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);
const DEFAULT_RESTART_BACKOFF: Duration = Duration::from_secs(1);
const DEFAULT_PERMISSION_DEFAULT_DENY: Duration = Duration::from_secs(120);
const DEFAULT_TOOL_EXECUTION: Duration = Duration::from_secs(300);
/// Above the 10 minute maximum of Claude Code's own Bash tool.
const DEFAULT_DELEGATED_TOOL: Duration = Duration::from_mins(15);

const TEST_STEP: Duration = Duration::from_millis(10);

/// Every time bound the daemon applies, passed to consumers so tests can shrink them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Timing {
    /// How long a backend may stay silent before it is considered stuck.
    #[serde(rename = "idle_timeout_ms", with = "millis")]
    pub idle_timeout: Duration,
    /// First delay between provider retries.
    #[serde(rename = "retry_backoff_base_ms", with = "millis")]
    pub retry_backoff_base: Duration,
    /// Upper bound for the provider retry delay.
    #[serde(rename = "retry_backoff_max_ms", with = "millis")]
    pub retry_backoff_max: Duration,
    /// Delay before restarting a crashed backend.
    #[serde(rename = "restart_backoff_ms", with = "millis")]
    pub restart_backoff: Duration,
    /// How long a permission prompt waits before it is denied.
    #[serde(rename = "permission_default_deny_ms", with = "millis")]
    pub permission_default_deny: Duration,
    /// Maximum run time of a single tool call.
    #[serde(rename = "tool_execution_ms", with = "millis")]
    pub tool_execution: Duration,
    /// How long a delegated backend (Claude Code, Codex) may stay silent while a tool it
    /// started on its own runs. Must not be zero.
    #[serde(rename = "delegated_tool_ms", with = "millis")]
    pub delegated_tool: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            retry_backoff_base: DEFAULT_RETRY_BACKOFF_BASE,
            retry_backoff_max: DEFAULT_RETRY_BACKOFF_MAX,
            restart_backoff: DEFAULT_RESTART_BACKOFF,
            permission_default_deny: DEFAULT_PERMISSION_DEFAULT_DENY,
            tool_execution: DEFAULT_TOOL_EXECUTION,
            delegated_tool: DEFAULT_DELEGATED_TOOL,
        }
    }
}

impl Timing {
    /// Tens of milliseconds for every bound, so timeout paths run fast in tests.
    #[must_use]
    pub const fn for_tests() -> Self {
        Self {
            idle_timeout: TEST_STEP.saturating_mul(5),
            retry_backoff_base: TEST_STEP,
            retry_backoff_max: TEST_STEP.saturating_mul(4),
            restart_backoff: TEST_STEP,
            permission_default_deny: TEST_STEP.saturating_mul(5),
            tool_execution: TEST_STEP.saturating_mul(10),
            delegated_tool: TEST_STEP.saturating_mul(10),
        }
    }
}

mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_millis)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Timing;

    #[test]
    fn for_tests_timings_are_all_under_one_second() {
        // Arrange
        let limit = Duration::from_secs(1);

        // Act
        let t = Timing::for_tests();

        // Assert
        for d in [
            t.idle_timeout,
            t.retry_backoff_base,
            t.retry_backoff_max,
            t.restart_backoff,
            t.permission_default_deny,
            t.tool_execution,
            t.delegated_tool,
        ] {
            assert!(d < limit, "{d:?} is not under 1s");
        }
    }
}
