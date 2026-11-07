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

/// The `[tui]` section.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// What to send to the terminal when it is out of focus and the UI needs
    /// the user. Defaults to the bell.
    pub notify: NotifyMode,
}
