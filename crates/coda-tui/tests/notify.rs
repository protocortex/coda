// SPDX-License-Identifier: Apache-2.0

//! The notification the UI writes to the terminal, driven by scripted focus
//! changes and an injected event stream.

use std::io;

use coda_config::tui::NotifyMode;
use coda_tui::app::{App, LinkEvent};
use coda_tui::permission::PermissionPrompt;
use coda_tui::{KeySource, UserInput, drive_with};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;
use tokio::sync::mpsc;

/// What the terminal does, in order; the approval arrives right after the first step.
struct Script {
    sender: mpsc::UnboundedSender<LinkEvent>,
    steps: Vec<Option<UserInput>>,
}

impl KeySource for Script {
    fn next_input(&mut self) -> io::Result<Option<UserInput>> {
        let step = if self.steps.is_empty() {
            None
        } else {
            self.steps.remove(0)
        };
        let sent = if step.is_some() || !self.steps.is_empty() {
            LinkEvent::Permission(prompt())
        } else {
            LinkEvent::Quit
        };
        self.sender.send(sent).unwrap();
        Ok(step)
    }
}

fn prompt() -> PermissionPrompt {
    PermissionPrompt::from_params(&json!({
        "session_id": "s-1",
        "request_id": "r-1",
        "tool": "bash",
        "repo_path": "/work/repo",
        "command": "ls"
    }))
    .unwrap()
}

/// Runs the UI over `steps` and returns what it wrote as notifications.
fn written(steps: Vec<Option<UserInput>>) -> String {
    let mut terminal = Terminal::new(TestBackend::new(40, 8)).unwrap();
    let mut app = App::new();
    app.set_notify(NotifyMode::Bell);
    let (sender, mut events) = mpsc::unbounded_channel();
    let mut keys = Script { sender, steps };
    let mut out = Vec::new();
    drive_with(&mut terminal, &mut app, &mut keys, &mut events, &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

#[test]
fn an_approval_that_arrives_after_the_terminal_lost_focus_rings_the_bell() {
    // Act
    let out = written(vec![Some(UserInput::Focus(false)), None]);

    // Assert
    assert_eq!(out, "\u{7}");
}

#[test]
fn an_approval_that_arrives_while_the_terminal_has_focus_writes_nothing() {
    // Act
    let out = written(vec![None, None]);

    // Assert
    assert_eq!(out, "");
}
