// SPDX-License-Identifier: AGPL-3.0-only

//! UI state, key handling and rendering.

use coda_protocol::events::Event;
use coda_protocol::types::BackendKind;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Paragraph};

use crate::chat::Chat;
use crate::input::Input;
use crate::status::Status;

const TITLE: &str = " coda (Esc quits) ";
/// Rows a page key moves the transcript.
const PAGE_ROWS: usize = 5;

/// Where the link to the daemon stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Failed(String),
    /// The link was up and then dropped.
    Disconnected(String),
}

/// Something that happened outside the key loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    Connected,
    Failed(String),
    Disconnected(String),
    /// A session event from the daemon.
    Session(Event),
    /// The process was asked to stop, for example by a signal.
    Quit,
}

#[derive(Debug)]
pub struct App {
    connection: ConnectionState,
    chat: Chat,
    input: Input,
    status: Status,
    submitted: Vec<String>,
    quit: bool,
}

impl App {
    #[must_use]
    pub fn new() -> Self {
        Self {
            connection: ConnectionState::Connecting,
            chat: Chat::new(),
            input: Input::new(),
            status: Status::new(),
            submitted: Vec::new(),
            quit: false,
        }
    }

    pub fn set_connection(&mut self, connection: ConnectionState) {
        self.connection = connection;
    }

    pub fn set_session(&mut self, backend: BackendKind, model: Option<String>) {
        self.status.set_session(backend, model);
    }

    /// Lines the user submitted since the last call, oldest first.
    pub fn take_submitted(&mut self) -> Vec<String> {
        std::mem::take(&mut self.submitted)
    }

    #[must_use]
    pub fn connection(&self) -> &ConnectionState {
        &self.connection
    }

    #[must_use]
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    pub fn handle_event(&mut self, event: LinkEvent) {
        match event {
            LinkEvent::Connected => self.connection = ConnectionState::Connected,
            LinkEvent::Failed(reason) => self.connection = ConnectionState::Failed(reason),
            LinkEvent::Disconnected(reason) => {
                self.connection = ConnectionState::Disconnected(reason);
            }
            LinkEvent::Session(event) => {
                self.chat.apply(&event);
                self.status.apply(&event);
            }
            LinkEvent::Quit => self.quit = true,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl_c || key.code == KeyCode::Esc {
            self.quit = true;
            return;
        }
        match key.code {
            KeyCode::PageUp => self.chat.scroll_up(PAGE_ROWS),
            KeyCode::PageDown => self.chat.scroll_down(PAGE_ROWS),
            _ => {
                if let Some(line) = self.input.handle_key(key) {
                    self.chat.push_user(line.clone());
                    self.submitted.push(line);
                }
            }
        }
    }

    pub fn handle_paste(&mut self, pasted: &str) {
        self.input.handle_paste(pasted);
    }

    pub fn render(&self, frame: &mut Frame) {
        let [body, input, connection, status] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let block = Block::bordered().title(TITLE);
        let transcript = block.inner(body);
        frame.render_widget(block, body);
        self.chat.render(frame, transcript);
        self.input.render(frame, input);
        frame.render_widget(Paragraph::new(self.connection_text()), connection);
        self.status.render(frame, status);
    }

    fn connection_text(&self) -> String {
        match &self.connection {
            ConnectionState::Connecting => "Connecting to the daemon".to_owned(),
            ConnectionState::Connected => "Connected to the daemon".to_owned(),
            ConnectionState::Failed(reason) => format!("Not connected: {reason}"),
            ConnectionState::Disconnected(reason) => {
                format!("Disconnected from the daemon: {reason}")
            }
        }
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use coda_protocol::types::{SessionId, TokenUsage, TurnId};

    use super::*;

    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 8;

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind,
            state: KeyEventState::NONE,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE, KeyEventKind::Press)
    }

    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT)).unwrap();
        terminal.draw(|frame| app.render(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .chunks(usize::from(WIDTH))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_new_app_shows_that_it_is_connecting() {
        // Arrange
        let app = App::new();

        // Act
        let screen = screen(&app);

        // Assert
        assert!(screen.contains("Connecting"), "{screen}");
        assert!(!app.should_quit());
    }

    #[test]
    fn a_connected_app_shows_that_it_is_connected() {
        // Arrange
        let mut app = App::new();

        // Act
        app.set_connection(ConnectionState::Connected);

        // Assert
        let screen = screen(&app);
        assert!(screen.contains("Connected"), "{screen}");
        assert!(!screen.contains("Connecting"), "{screen}");
    }

    #[test]
    fn a_failed_connection_shows_the_reason() {
        // Arrange
        let mut app = App::new();

        // Act
        app.set_connection(ConnectionState::Failed("socket missing".to_owned()));

        // Assert
        let screen = screen(&app);
        assert!(screen.contains("socket missing"), "{screen}");
    }

    #[test]
    fn link_events_update_the_connection_and_can_quit() {
        for (event, expected) in [
            (LinkEvent::Connected, ConnectionState::Connected),
            (
                LinkEvent::Failed("no socket".to_owned()),
                ConnectionState::Failed("no socket".to_owned()),
            ),
            (
                LinkEvent::Disconnected("closed".to_owned()),
                ConnectionState::Disconnected("closed".to_owned()),
            ),
        ] {
            // Arrange
            let mut app = App::new();

            // Act
            app.handle_event(event);

            // Assert
            assert_eq!(app.connection(), &expected);
            assert!(!app.should_quit());
        }
    }

    #[test]
    fn a_disconnected_app_shows_the_reason() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_event(LinkEvent::Disconnected("EOF".to_owned()));

        // Assert
        let screen = screen(&app);
        assert!(screen.contains("Disconnected"), "{screen}");
        assert!(screen.contains("EOF"), "{screen}");
    }

    #[test]
    fn a_quit_event_ends_the_app() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_event(LinkEvent::Quit);

        // Assert
        assert!(app.should_quit());
    }

    #[test]
    fn the_quit_keys_end_the_app() {
        for (code, modifiers) in [
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            // Arrange
            let mut app = App::new();

            // Act
            app.handle_key(key(code, modifiers, KeyEventKind::Press));

            // Assert
            assert!(app.should_quit(), "{code:?} {modifiers:?}");
        }
    }

    #[test]
    fn other_keys_and_key_releases_do_not_end_the_app() {
        for event in [
            press(KeyCode::Char('x')),
            press(KeyCode::Char('q')),
            key(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Release),
        ] {
            // Arrange
            let mut app = App::new();

            // Act
            app.handle_key(event);

            // Assert
            assert!(!app.should_quit(), "{event:?}");
        }
    }

    fn delta(text: &str) -> LinkEvent {
        LinkEvent::Session(Event::StreamDelta {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            text: text.to_owned(),
        })
    }

    fn type_line(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
        app.handle_key(press(KeyCode::Enter));
    }

    #[test]
    fn streamed_deltas_render_in_the_transcript() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_event(delta("Hel"));
        app.handle_event(delta("lo"));

        // Assert
        let screen = screen(&app);
        assert!(screen.contains("Hello"), "{screen}");
    }

    #[test]
    fn the_status_line_shows_backend_model_and_tokens() {
        // Arrange
        let mut app = App::new();

        // Act
        app.set_session(BackendKind::Faux, Some("m-1".to_owned()));
        app.handle_event(LinkEvent::Session(Event::Usage {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            usage: TokenUsage {
                input_tokens: 7,
                output_tokens: 3,
                ..TokenUsage::default()
            },
        }));

        // Assert
        let screen = screen(&app);
        assert!(screen.contains("faux | m-1 | in 7 out 3"), "{screen}");
    }

    #[test]
    fn a_submitted_line_enters_the_transcript_and_the_outbox_once() {
        // Arrange
        let mut app = App::new();

        // Act
        type_line(&mut app, "q hi");
        let first = app.take_submitted();
        let second = app.take_submitted();

        // Assert
        assert_eq!(first, vec!["q hi".to_owned()]);
        assert_eq!(second, Vec::<String>::new());
        assert!(!app.should_quit());
        let screen = screen(&app);
        assert!(screen.contains("you: q hi"), "{screen}");
    }

    #[test]
    fn the_page_keys_scroll_the_transcript() {
        // Arrange
        let mut app = App::new();
        let lines: Vec<String> = (1..=30).map(|n| format!("row{n:02}")).collect();
        app.handle_event(delta(&lines.join("\n")));
        let bottom = screen(&app);

        // Act
        app.handle_key(press(KeyCode::PageUp));
        let scrolled = screen(&app);
        app.handle_key(press(KeyCode::PageDown));
        let back = screen(&app);

        // Assert
        assert!(bottom.contains("row30"), "{bottom}");
        assert!(!scrolled.contains("row30"), "{scrolled}");
        assert_eq!(back, bottom);
    }
}
