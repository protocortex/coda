// SPDX-License-Identifier: Apache-2.0

//! The `[tui]` table: settings of the terminal UI.

use serde::Deserialize;

/// How the terminal UI tells the user that a turn ended or an approval waits
/// while the terminal does not have focus.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyMode {
    /// No notification.
    Off,
    /// The terminal bell.
    #[default]
    Bell,
    /// An OSC 9 desktop notification, as iTerm2 and others read it.
    Osc9,
    /// An OSC 777 desktop notification, as VTE terminals and others read it.
    Osc777,
}

/// How long Claude Code's prompt cache is assumed to last when nothing is sent.
pub const DEFAULT_CACHE_TTL_SECS: u64 = 300;
/// The smallest context, in tokens, for which the UI warns that a cold cache is costly.
pub const DEFAULT_CACHE_HINT_TOKENS: u64 = 50_000;

/// The `[tui]` section.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// What to send to the terminal when it is out of focus and the UI needs
    /// the user. Defaults to the bell.
    pub notify: NotifyMode,
    /// Seconds without a message after which the prompt cache is assumed gone.
    /// Zero turns the cache hint off.
    pub cache_ttl_secs: u64,
    /// The smallest context, in tokens, that gets the cache hint.
    pub cache_hint_tokens: u64,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            notify: NotifyMode::default(),
            cache_ttl_secs: DEFAULT_CACHE_TTL_SECS,
            cache_hint_tokens: DEFAULT_CACHE_HINT_TOKENS,
        }
    }
}
