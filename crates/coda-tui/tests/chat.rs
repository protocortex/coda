// SPDX-License-Identifier: AGPL-3.0-only

//! The chat view driven by an injected event stream and scripted keys.
//! The events are synthetic, not recorded from a live backend.

use std::collections::VecDeque;
use std::io;

use coda_protocol::events::Event;
use coda_protocol::types::{BackendKind, SessionId, TokenUsage, TurnId};
use coda_tui::app::{App, LinkEvent};
use coda_tui::{KeySource, UserInput};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::sync::mpsc;

const WIDTH: u16 = 40;
const HEIGHT: u16 = 8;

struct Keys(VecDeque<UserInput>);

impl Keys {
    fn codes(codes: impl IntoIterator<Item = KeyCode>) -> Self {
        Self(codes.into_iter().map(key).collect())
    }
}

fn key(code: KeyCode) -> UserInput {
    UserInput::Key(KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

impl KeySource for Keys {
    fn next_input(&mut self) -> io::Result<Option<UserInput>> {
        Ok(self.0.pop_front())
    }
}

fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(WIDTH))
        .map(|row| {
            row.iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

#[test]
fn streamed_deltas_and_usage_reach_the_screen_through_the_event_channel() {
    // Arrange
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
    let mut app = App::new();
    app.set_session(BackendKind::Faux, Some("m-1".to_owned()));
    let (sender, mut events) = mpsc::unbounded_channel();
    for text in ["Stream", "ed ", "reply"] {
        sender
            .send(LinkEvent::Session(Event::StreamDelta {
                session_id: SessionId::new("s-1"),
                turn_id: TurnId::new("t-1"),
                text: text.to_owned(),
            }))
            .unwrap();
    }
    sender
        .send(LinkEvent::Session(Event::Usage {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            usage: TokenUsage {
                input_tokens: 12,
                output_tokens: 34,
                ..TokenUsage::default()
            },
        }))
        .unwrap();
    let mut keys = Keys::codes([KeyCode::Char('h'), KeyCode::Esc]);

    // Act
    coda_tui::drive(&mut terminal, &mut app, &mut keys, &mut events).unwrap();

    // Assert
    let rows = rows(&terminal);
    assert!(
        rows.iter().any(|row| row.contains("Streamed reply")),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row == "faux | m-1 | in 12 out 34"),
        "{rows:?}"
    );
    assert!(rows.iter().any(|row| row == "> h"), "{rows:?}");
}

#[test]
fn a_one_cell_screen_survives_a_busy_session() {
    // Arrange
    let mut terminal = Terminal::new(TestBackend::new(1, 1)).unwrap();
    let mut app = App::new();
    let (sender, mut events) = mpsc::unbounded_channel();
    sender
        .send(LinkEvent::Session(Event::StreamDelta {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            text: "wide \u{4e2d}\u{6587} text ".repeat(50),
        }))
        .unwrap();
    let mut keys = Keys::codes([KeyCode::Char('\u{4e2d}'), KeyCode::PageUp, KeyCode::Esc]);

    // Act
    let result = coda_tui::drive(&mut terminal, &mut app, &mut keys, &mut events);

    // Assert
    assert!(result.is_ok());
    assert!(app.should_quit());
}

#[test]
fn a_paste_with_newlines_fills_the_prompt_and_only_enter_submits_it() {
    // Arrange
    let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
    let (link, mut commands) = mpsc::unbounded_channel();
    let mut app = App::with_link(link);
    let (_sender, mut events) = mpsc::unbounded_channel();
    let mut keys = Keys(
        [
            UserInput::Paste("first\nsecond\n".to_owned()),
            key(KeyCode::Esc),
        ]
        .into(),
    );

    // Act
    coda_tui::drive(&mut terminal, &mut app, &mut keys, &mut events).unwrap();

    // Assert
    assert!(commands.try_recv().is_err());
    let rows = rows(&terminal);
    assert!(rows.iter().any(|row| row == "> first second"), "{rows:?}");
}
