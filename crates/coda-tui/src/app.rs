// SPDX-License-Identifier: AGPL-3.0-only

//! UI state, key handling and rendering.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Paragraph};

const TITLE: &str = " coda ";
const QUIT_HINT: &str = "q to quit";

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
    /// The process was asked to stop, for example by a signal.
    Quit,
}

#[derive(Debug)]
pub struct App {
    connection: ConnectionState,
    quit: bool,
}

impl App {
    #[must_use]
    pub fn new() -> Self {
        Self {
            connection: ConnectionState::Connecting,
            quit: false,
        }
    }

    pub fn set_connection(&mut self, connection: ConnectionState) {
        self.connection = connection;
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
            LinkEvent::Quit => self.quit = true,
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl_c =
            key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
            self.quit = true;
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let [body, footer] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
        let status = match &self.connection {
            ConnectionState::Connecting => "Connecting to the daemon".to_owned(),
            ConnectionState::Connected => "Connected to the daemon".to_owned(),
            ConnectionState::Failed(reason) => format!("Not connected: {reason}"),
            ConnectionState::Disconnected(reason) => {
                format!("Disconnected from the daemon: {reason}")
            }
        };
        frame.render_widget(
            Paragraph::new(status).block(Block::bordered().title(TITLE)),
            body,
        );
        frame.render_widget(Paragraph::new(QUIT_HINT), footer);
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

    use super::*;

    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 5;

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
            (KeyCode::Char('q'), KeyModifiers::NONE),
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
            key(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
        ] {
            // Arrange
            let mut app = App::new();

            // Act
            app.handle_key(event);

            // Assert
            assert!(!app.should_quit(), "{event:?}");
        }
    }
}
