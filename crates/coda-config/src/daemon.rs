// SPDX-License-Identifier: Apache-2.0

//! The `[daemon]` table: how the daemon treats work a restart cut short.

use serde::Deserialize;

/// The `[daemon]` section.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Queue a continuation for each turn a restart cut short and start it once
    /// the daemon is up. Off by default.
    pub continue_after_restart: bool,
}
