// SPDX-License-Identifier: Apache-2.0

//! UI state, key handling and rendering.

use std::time::{Duration, Instant};

use coda_protocol::events::Event;
use coda_protocol::methods::{SessionStatus, SessionSummary};
use coda_protocol::types::{BackendKind, SessionInfo};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Paragraph};
use tokio::sync::mpsc::UnboundedSender;

use crate::chat::Chat;
use crate::commands::{self, Command, CommandError, Resolved, UserCommands};
use crate::input::Input;
use crate::permission::{Overlay, PermissionPrompt};
use crate::sessions::LinkCommand;
use crate::status::Status;

const TITLE: &str = " coda (/quit to exit) ";
/// How long after an idle Ctrl+C a second one still quits.
const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
const QUIT_CONFIRM_NOTICE: &str = "Press Ctrl+C again to quit.";
/// Shown after Ctrl+C in a running turn, where a second press cancels again.
const BUSY_HINT: &str = "Cancelling. Esc then Ctrl+D to quit.";
/// Rows a page key moves the transcript.
const PAGE_ROWS: usize = 5;
/// Shown in the transcript when no provider is enabled in the config.
pub const SETUP_GUIDANCE: &str = "No provider is enabled. Run `coda init`, enable one in config.toml, then run `coda daemon restart`.";
/// Closes the connection line after the daemon went away.
pub const LOST_DAEMON_HINT: &str = "Ctrl+D quits. Run `coda` to start the daemon again.";
const NO_SESSIONS: &str = "the daemon has no sessions";
const BACKEND_SWITCH_BUSY: &str = "wait for the running turn to finish before switching backend";

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
    /// A tool is waiting for the user's permission.
    Permission(PermissionPrompt),
    /// The session the UI follows changed; its history is about to replay.
    SessionStarted(SessionInfo),
    /// The daemon switched the model of the session the UI follows.
    ModelSwitched(SessionInfo),
    /// The user's own message, live or replayed from the stored history.
    UserMessage(String),
    /// The daemon's sessions, answering `/sessions`.
    Sessions(Vec<SessionSummary>),
    /// Something the user should read, such as a failed request.
    Notice(String),
    /// The daemon has no enabled provider.
    SetupNeeded,
    /// The process was asked to stop, for example by a signal.
    Quit,
}

#[derive(Debug)]
pub struct App {
    connection: ConnectionState,
    chat: Chat,
    input: Input,
    status: Status,
    overlay: Overlay,
    link: Option<UnboundedSender<LinkCommand>>,
    turn_running: bool,
    setup_shown: bool,
    user_commands: UserCommands,
    quit: bool,
    idle_ctrl_c_at: Option<Instant>,
    /// A short-lived line that replaces the connection line.
    hint: Option<(Instant, &'static str)>,
}

impl App {
    #[must_use]
    pub fn new() -> Self {
        Self {
            connection: ConnectionState::Connecting,
            chat: Chat::new(),
            input: Input::new(),
            status: Status::new(),
            overlay: Overlay::default(),
            link: None,
            turn_running: false,
            setup_shown: false,
            user_commands: UserCommands::default(),
            quit: false,
            idle_ctrl_c_at: None,
            hint: None,
        }
    }

    /// An app whose messages, cancels and answers go to `link`.
    #[must_use]
    pub fn with_link(link: UnboundedSender<LinkCommand>) -> Self {
        Self {
            link: Some(link),
            ..Self::new()
        }
    }

    /// Offers the commands from command files; a failure to read them, files
    /// that were skipped and a name a built-in already has are reported in the
    /// transcript.
    pub fn set_user_commands(&mut self, commands: UserCommands) {
        for notice in commands.startup_notices() {
            self.chat.push_notice(notice);
        }
        self.user_commands = commands;
    }

    /// Shows the setup guidance, once however many sources ask for it.
    pub fn show_setup_guidance(&mut self) {
        if !std::mem::replace(&mut self.setup_shown, true) {
            self.chat.push_notice(SETUP_GUIDANCE);
        }
    }

    pub fn set_connection(&mut self, connection: ConnectionState) {
        self.connection = connection;
    }

    pub fn set_session(&mut self, backend: BackendKind, model: Option<String>) {
        self.status.set_session(backend, model);
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
                self.overlay = Overlay::default();
                self.turn_running = false;
            }
            LinkEvent::Session(event) => self.apply_session_event(&event),
            LinkEvent::Permission(prompt) => self.overlay.push(prompt, Instant::now()),
            LinkEvent::SessionStarted(info) => {
                self.chat.clear();
                self.status.set_session(info.backend, info.model);
                self.overlay = Overlay::default();
                self.turn_running = false;
            }
            LinkEvent::ModelSwitched(info) => {
                self.chat.push_notice(format!(
                    "model is now {}",
                    info.model.as_deref().unwrap_or("the default")
                ));
                self.status.switch_model(info.model);
            }
            LinkEvent::UserMessage(text) => self.chat.push_user(text),
            LinkEvent::Sessions(sessions) => self.show_sessions(&sessions),
            LinkEvent::Notice(text) => self.chat.push_notice(text),
            LinkEvent::SetupNeeded => self.show_setup_guidance(),
            LinkEvent::Quit => self.quit = true,
        }
    }

    fn apply_session_event(&mut self, event: &Event) {
        match event {
            Event::Approve { request_id, .. }
            | Event::Deny { request_id, .. }
            | Event::Timeout { request_id, .. } => self.overlay.resolve(request_id),
            Event::Result { .. } | Event::TurnInterrupted { .. } => self.turn_running = false,
            // A restart keeps the same turn going, so only a final crash ends it.
            Event::BackendCrashed { will_restart, .. } => {
                self.turn_running &= *will_restart;
            }
            _ => {}
        }
        self.chat.apply(event);
        self.status.apply(event);
    }

    /// Sends to the daemon link. A dead link can no longer answer prompts or
    /// end turns, so the UI drops them and says so. Returns whether it was sent.
    fn send(&mut self, command: LinkCommand) -> bool {
        let failed = self
            .link
            .as_ref()
            .is_some_and(|link| link.send(command).is_err());
        if failed {
            self.chat.push_notice(
                "the link to the daemon is closed, the request was not sent".to_owned(),
            );
            self.overlay = Overlay::default();
            self.turn_running = false;
        }
        !failed
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        self.handle_key_at(key, Instant::now());
    }

    fn handle_key_at(&mut self, key: KeyEvent, now: Instant) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.interrupt(now);
            return;
        }
        self.idle_ctrl_c_at = None;
        self.hint = None;
        if self.overlay.is_active() {
            if let Some(answer) = self.overlay.handle_key(key, now) {
                self.send(LinkCommand::Respond(answer));
            }
        } else if ctrl && key.code == KeyCode::Char('d') {
            let lost = matches!(self.connection, ConnectionState::Disconnected(_));
            if lost || self.input.text().is_empty() {
                self.quit = true;
            }
        } else if key.code == KeyCode::Esc {
            self.input.clear();
        } else {
            self.handle_prompt_key(key);
        }
    }

    /// Denies pending prompts and cancels the running turn. Idle, the first
    /// press asks for confirmation and a second one inside the window quits.
    /// The turn counts as over only when the daemon says so.
    fn interrupt(&mut self, now: Instant) {
        let denials = self.overlay.deny_all();
        let busy = self.turn_running || !denials.is_empty();
        for answer in denials {
            self.send(LinkCommand::Respond(answer));
        }
        if self.turn_running {
            self.send(LinkCommand::Cancel);
        }
        if busy {
            self.idle_ctrl_c_at = None;
            if self.turn_running {
                self.hint = Some((now, BUSY_HINT));
            }
            return;
        }
        let confirmed = self
            .idle_ctrl_c_at
            .is_some_and(|first| now.duration_since(first) <= QUIT_CONFIRM_WINDOW);
        if confirmed {
            self.quit = true;
        } else {
            self.idle_ctrl_c_at = Some(now);
            self.hint = Some((now, QUIT_CONFIRM_NOTICE));
        }
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::PageUp => self.chat.scroll_up(PAGE_ROWS),
            KeyCode::PageDown => self.chat.scroll_down(PAGE_ROWS),
            _ => {
                if let Some(line) = self.input.handle_key(key) {
                    self.submit(&line);
                }
            }
        }
    }

    fn submit(&mut self, line: &str) {
        match commands::parse(line) {
            Ok(None) => match self.user_commands.mistyped(line) {
                Some(word) => self
                    .chat
                    .push_notice(CommandError::Unknown(word).to_string()),
                None => self.send_message(line, line),
            },
            Ok(Some(command)) => self.run_command(command),
            // A built-in always wins, so only a word it does not know can be a command file.
            Err(CommandError::Unknown(word)) => self.run_user_command(&word, line),
            Err(error) => self.chat.push_notice(error.to_string()),
        }
    }

    /// Sends `text` as the user's message. `typed` is what to restore to the
    /// input when the link is closed, so it can be sent again.
    fn send_message(&mut self, text: &str, typed: &str) {
        self.turn_running = true;
        if !self.send(LinkCommand::Send(text.to_owned())) {
            self.input.handle_paste(typed);
        }
    }

    fn run_user_command(&mut self, word: &str, line: &str) {
        let args = line.trim_start().strip_prefix(word).unwrap_or_default();
        match self.user_commands.resolve(word, args) {
            Resolved::Prompt(prompt) => self.send_message(&prompt, line),
            Resolved::Refused(reason) => self.chat.push_notice(reason),
            Resolved::Unknown => self
                .chat
                .push_notice(CommandError::Unknown(word.to_owned()).to_string()),
        }
    }

    fn show_help(&mut self) {
        for line in self.user_commands.help_lines() {
            self.chat.push_notice(line);
        }
    }

    fn run_command(&mut self, command: Command) {
        let link_command = match command {
            Command::Sessions => LinkCommand::ListSessions,
            Command::New => LinkCommand::New,
            Command::Resume(id) => LinkCommand::Resume(id),
            Command::Model(alias) => LinkCommand::SetModel(alias),
            Command::Backend(None) => LinkCommand::ListBackends,
            Command::Backend(Some(_)) if self.turn_running => {
                self.chat.push_notice(BACKEND_SWITCH_BUSY.to_owned());
                return;
            }
            Command::Backend(Some(name)) => LinkCommand::SwitchBackend(name),
            Command::Help => {
                self.show_help();
                return;
            }
            Command::Quit => {
                self.quit = true;
                return;
            }
        };
        self.send(link_command);
    }

    fn show_sessions(&mut self, sessions: &[SessionSummary]) {
        if sessions.is_empty() {
            self.chat.push_notice(NO_SESSIONS.to_owned());
        }
        for session in sessions {
            self.chat.push_notice(session_line(session));
        }
    }

    pub fn handle_paste(&mut self, pasted: &str) {
        self.idle_ctrl_c_at = None;
        self.hint = None;
        self.input.handle_paste(pasted);
    }

    pub fn render(&self, frame: &mut Frame) {
        self.render_at(frame, Instant::now());
    }

    fn render_at(&self, frame: &mut Frame, now: Instant) {
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
        frame.render_widget(Paragraph::new(self.connection_text(now)), connection);
        self.status.render(frame, status);
        self.overlay.render(frame, frame.area());
    }

    fn connection_text(&self, now: Instant) -> String {
        let lost = matches!(self.connection, ConnectionState::Disconnected(_));
        if let Some((shown_at, hint)) = self.hint
            && !lost
            && now.duration_since(shown_at) <= QUIT_CONFIRM_WINDOW
        {
            return hint.to_owned();
        }
        match &self.connection {
            ConnectionState::Connecting => "Connecting to the daemon".to_owned(),
            ConnectionState::Connected => "Connected to the daemon".to_owned(),
            ConnectionState::Failed(reason) => format!("Not connected: {reason}"),
            ConnectionState::Disconnected(reason) => {
                format!("{LOST_DAEMON_HINT} Disconnected from the daemon: {reason}")
            }
        }
    }
}

fn session_line(session: &SessionSummary) -> String {
    let status = match session.status {
        SessionStatus::Running => "running",
        SessionStatus::Idle => "idle",
        _ => "unknown",
    };
    format!("{} | {status} | {}", session.id, session.cwd)
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

    use coda_protocol::methods::{SessionStatus, SessionSummary};
    use coda_protocol::types::{PermissionDecision, RequestId, SessionId, TokenUsage, TurnId};
    use tokio::sync::mpsc::UnboundedReceiver;

    use crate::permission::PermissionAnswer;

    use super::*;

    const WIDTH: u16 = 40;
    const HEIGHT: u16 = 12;

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
        let screen = wide_screen(&app);
        assert!(screen.contains("Disconnected"), "{screen}");
        assert!(screen.contains("EOF"), "{screen}");
    }

    #[test]
    fn a_lost_daemon_says_how_to_recover() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_event(LinkEvent::Disconnected("EOF".to_owned()));

        // Assert
        let screen = wide_screen(&app);
        assert!(screen.contains(LOST_DAEMON_HINT), "{screen}");
    }

    #[test]
    fn setup_guidance_says_to_restart_the_daemon() {
        // Arrange / Act / Assert
        assert!(SETUP_GUIDANCE.contains("coda daemon restart"));
    }

    #[test]
    fn setup_guidance_is_shown_only_when_asked_for() {
        for (needs_setup, shown) in [(true, true), (false, false)] {
            // Arrange
            let mut app = App::new();

            // Act
            if needs_setup {
                app.show_setup_guidance();
            }

            // Assert
            let screen = wide_screen(&app);
            assert_eq!(screen.contains(SETUP_GUIDANCE), shown, "{screen}");
        }
    }

    #[test]
    fn a_setup_needed_event_shows_the_guidance_once() {
        // Arrange
        let mut app = App::new();
        app.show_setup_guidance();

        // Act
        app.handle_event(LinkEvent::SetupNeeded);
        app.handle_event(LinkEvent::SetupNeeded);

        // Assert
        assert_eq!(wide_screen(&app).matches(SETUP_GUIDANCE).count(), 1);
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

    fn ctrl_d() -> KeyEvent {
        key(
            KeyCode::Char('d'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        )
    }

    #[test]
    fn esc_never_quits() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_key(press(KeyCode::Esc));

        // Assert
        assert!(!app.should_quit());
    }

    #[test]
    fn esc_clears_the_input() {
        // Arrange
        let mut app = App::new();
        app.handle_key(press(KeyCode::Char('h')));
        app.handle_key(press(KeyCode::Char('i')));

        // Act
        app.handle_key(press(KeyCode::Esc));

        // Assert
        assert_eq!(app.input.text(), "");
        assert!(!app.should_quit());
    }

    #[test]
    fn esc_closes_an_overlay_and_does_not_quit() {
        // Arrange
        let (mut app, mut commands) = armed();
        app.handle_event(prompt("r-1"));

        // Act
        app.handle_key(press(KeyCode::Esc));

        // Assert
        assert!(!app.should_quit());
        assert!(!app.overlay.is_active());
        assert_eq!(sent(&mut commands).len(), 1);
    }

    #[test]
    fn slash_quit_ends_the_app() {
        // Arrange
        let mut app = App::new();

        // Act
        type_line(&mut app, "/quit");

        // Assert
        assert!(app.should_quit());
    }

    #[test]
    fn ctrl_d_quits_only_on_an_empty_input() {
        // Arrange
        let mut app = App::new();
        app.handle_key(press(KeyCode::Char('x')));

        // Act
        app.handle_key(ctrl_d());
        let with_text = app.should_quit();
        app.handle_key(press(KeyCode::Backspace));
        app.handle_key(ctrl_d());

        // Assert
        assert!(!with_text);
        assert!(app.should_quit());
    }

    #[test]
    fn a_double_ctrl_c_when_idle_quits() {
        // Arrange
        let mut app = App::new();
        let start = Instant::now();

        // Act
        app.handle_key_at(ctrl_c(), start);
        let after_first = app.should_quit();
        app.handle_key_at(ctrl_c(), start + QUIT_CONFIRM_WINDOW);

        // Assert
        assert!(!after_first);
        assert!(screen(&app).contains(QUIT_CONFIRM_NOTICE));
        assert!(app.should_quit());
    }

    #[test]
    fn a_second_ctrl_c_after_the_window_only_asks_again() {
        // Arrange
        let mut app = App::new();
        let start = Instant::now();

        // Act
        app.handle_key_at(ctrl_c(), start);
        app.handle_key_at(
            ctrl_c(),
            start + QUIT_CONFIRM_WINDOW + Duration::from_millis(1),
        );

        // Assert
        assert!(!app.should_quit());
    }

    #[test]
    fn another_key_between_two_ctrl_c_presses_resets_the_confirmation() {
        // Arrange
        let mut app = App::new();
        let start = Instant::now();

        // Act
        app.handle_key_at(ctrl_c(), start);
        app.handle_key_at(press(KeyCode::Char('x')), start);
        app.handle_key_at(ctrl_c(), start);

        // Assert
        assert!(!app.should_quit());
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

    fn linked() -> (App, UnboundedReceiver<LinkCommand>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (App::with_link(sender), receiver)
    }

    fn sent(commands: &mut UnboundedReceiver<LinkCommand>) -> Vec<LinkCommand> {
        std::iter::from_fn(|| commands.try_recv().ok()).collect()
    }

    /// An app whose prompts take answers at once, as after the grace period.
    fn armed() -> (App, UnboundedReceiver<LinkCommand>) {
        let (mut app, commands) = linked();
        app.overlay = Overlay::with_grace(std::time::Duration::ZERO);
        (app, commands)
    }

    fn turn_ended() -> LinkEvent {
        LinkEvent::Session(Event::TurnInterrupted {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            cause: None,
        })
    }

    fn ctrl_c() -> KeyEvent {
        key(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        )
    }

    fn prompt(request: &str) -> LinkEvent {
        LinkEvent::Permission(PermissionPrompt {
            session_id: SessionId::new("s-1"),
            request_id: RequestId::new(request),
            tool: "bash".to_owned(),
            repo_path: "/work/repo".to_owned(),
            command: "cargo test".to_owned(),
            agent: None,
            truncated: false,
        })
    }

    fn session(id: &str) -> SessionInfo {
        SessionInfo {
            id: SessionId::new(id),
            backend: BackendKind::Faux,
            model: Some("m-1".to_owned()),
            cwd: "/work".to_owned(),
            mode: coda_protocol::types::Mode::Default,
        }
    }

    #[test]
    fn a_submitted_line_is_sent_once_and_enters_the_transcript_when_the_daemon_echoes_it() {
        // Arrange
        let (mut app, mut commands) = linked();

        // Act
        type_line(&mut app, "q hi");

        // Assert
        assert_eq!(
            sent(&mut commands),
            vec![LinkCommand::Send("q hi".to_owned())]
        );
        assert!(!app.should_quit());
        assert!(!screen(&app).contains("you: q hi"));
        app.handle_event(LinkEvent::UserMessage("q hi".to_owned()));
        let screen = screen(&app);
        assert!(screen.contains("you: q hi"), "{screen}");
    }

    #[test]
    fn ctrl_c_cancels_a_running_turn_and_quits_only_once_the_daemon_ended_it() {
        // Arrange
        let (mut app, mut commands) = linked();
        type_line(&mut app, "go");
        sent(&mut commands);

        // Act
        app.handle_key(ctrl_c());
        app.handle_key(ctrl_c());
        let before_end = (app.should_quit(), sent(&mut commands));
        app.handle_event(turn_ended());
        app.handle_key(ctrl_c());
        let after_first_idle = app.should_quit();
        app.handle_key(ctrl_c());

        // Assert
        assert!(!after_first_idle);
        assert_eq!(
            before_end,
            (false, vec![LinkCommand::Cancel, LinkCommand::Cancel])
        );
        assert!(app.should_quit());
        assert_eq!(sent(&mut commands), vec![]);
    }

    #[test]
    fn ctrl_c_denies_every_pending_prompt_and_clears_the_overlay() {
        // Arrange
        let (mut app, mut commands) = armed();
        app.handle_event(prompt("r-1"));
        app.handle_event(prompt("r-2"));

        // Act
        app.handle_key(ctrl_c());

        // Assert
        let denied: Vec<_> = sent(&mut commands)
            .into_iter()
            .map(|c| match c {
                LinkCommand::Respond(a) => (a.request_id, a.decision),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            denied,
            vec![
                (RequestId::new("r-1"), PermissionDecision::Deny),
                (RequestId::new("r-2"), PermissionDecision::Deny)
            ]
        );
        assert!(!app.overlay.is_active());
        assert!(!app.should_quit());
    }

    #[test]
    fn a_dead_link_clears_the_overlay_and_the_running_turn_and_says_so() {
        // Arrange
        let (mut app, commands) = armed();
        app.handle_event(prompt("r-1"));
        screen(&app);
        drop(commands);

        // Act
        app.handle_key(press(KeyCode::Char('y')));
        type_line(&mut app, "go");

        // Assert
        assert!(!app.overlay.is_active());
        assert!(!app.turn_running);
        let shown = screen(&app);
        assert!(shown.contains("link to the daemon"), "{shown}");
    }

    #[test]
    fn a_permission_request_takes_the_keys_and_the_answer_carries_both_ids() {
        // Arrange
        let (mut app, mut commands) = armed();
        app.handle_event(prompt("r-1"));
        let drawn = screen(&app);

        // Act
        app.handle_key(press(KeyCode::Char('q')));
        app.handle_key(press(KeyCode::Char('a')));
        app.handle_key(press(KeyCode::Enter));

        // Assert
        assert!(drawn.contains("cargo test"), "{drawn}");
        let shown = screen(&app);
        assert!(!shown.contains("cargo test"), "{shown}");
        assert!(!app.should_quit());
        assert_eq!(
            sent(&mut commands),
            vec![LinkCommand::Respond(PermissionAnswer {
                session_id: SessionId::new("s-1"),
                request_id: RequestId::new("r-1"),
                decision: PermissionDecision::AllowAlways,
            })]
        );
    }

    #[test]
    fn the_overlay_goes_when_the_request_is_decided_elsewhere_or_the_link_or_session_ends() {
        for event in [
            LinkEvent::Session(Event::Timeout {
                session_id: SessionId::new("s-1"),
                request_id: RequestId::new("r-1"),
                tool: "Bash".to_owned(),
            }),
            LinkEvent::Disconnected("EOF".to_owned()),
            LinkEvent::SessionStarted(session("s-2")),
        ] {
            // Arrange
            let mut app = App::new();
            app.handle_event(prompt("r-1"));

            // Act
            app.handle_event(event);

            // Assert
            assert!(!app.overlay.is_active());
        }
    }

    #[test]
    fn slash_commands_go_to_the_link_and_never_to_the_model() {
        // Arrange
        let (mut app, mut commands) = linked();

        // Act
        for line in ["/new", "/resume s-2", "/bogus"] {
            type_line(&mut app, line);
        }

        // Assert
        assert_eq!(
            sent(&mut commands),
            vec![LinkCommand::New, LinkCommand::Resume(SessionId::new("s-2"))]
        );
        let shown = screen(&app);
        assert!(shown.contains("unknown command /bogus"), "{shown}");
    }

    #[test]
    fn a_line_that_cannot_be_sent_stays_in_the_input_and_no_turn_is_running() {
        // Arrange
        let (mut app, commands) = linked();
        drop(commands);

        // Act
        type_line(&mut app, "keep me");

        // Assert
        assert!(!app.turn_running);
        assert_eq!(app.input.text(), "keep me");
        let shown = screen(&app);
        assert!(shown.contains("request was not sent"), "{shown}");
    }

    #[test]
    fn a_started_session_replaces_the_transcript_and_status() {
        // Arrange
        let mut app = App::new();
        app.handle_event(delta("old text"));

        // Act
        app.handle_event(LinkEvent::SessionStarted(session("s-2")));
        app.handle_event(LinkEvent::Notice("could not send".to_owned()));

        // Assert
        let shown = screen(&app);
        assert!(!shown.contains("old text"), "{shown}");
        assert!(shown.contains("faux | m-1"), "{shown}");
        assert!(shown.contains("could not send"), "{shown}");
    }

    #[test]
    fn a_replayed_user_message_and_a_session_list_are_shown_in_the_transcript() {
        // Arrange
        let mut app = App::new();
        let summary = SessionSummary {
            id: SessionId::new("s-7"),
            backend: BackendKind::Faux,
            cwd: "/work".to_owned(),
            created_at: 1,
            updated_at: 2,
            status: SessionStatus::Running,
        };

        // Act
        app.handle_event(LinkEvent::UserMessage("hello again".to_owned()));
        app.handle_event(LinkEvent::Sessions(vec![summary]));

        // Assert
        let shown = screen(&app);
        assert!(shown.contains("hello again"), "{shown}");
        assert!(shown.contains("s-7"), "{shown}");
        assert!(shown.contains("running"), "{shown}");
    }

    fn wide_screen(app: &App) -> String {
        crate::test_support::screen(120, 8, |frame| app.render(frame))
    }

    fn crashed(will_restart: bool) -> LinkEvent {
        LinkEvent::Session(Event::BackendCrashed {
            session_id: SessionId::new("s-1"),
            exit_code: Some(3),
            will_restart,
            detail: None,
        })
    }

    #[test]
    fn a_crash_that_is_followed_by_a_restart_keeps_the_turn_running() {
        // Arrange
        let (mut app, _commands) = linked();
        type_line(&mut app, "go");

        // Act
        app.handle_event(crashed(true));

        // Assert
        assert!(app.turn_running);
    }

    #[test]
    fn a_crash_with_no_restart_ends_the_turn() {
        // Arrange
        let (mut app, _commands) = linked();
        type_line(&mut app, "go");

        // Act
        app.handle_event(crashed(false));

        // Assert
        assert!(!app.turn_running);
    }

    #[test]
    fn switch_commands_go_to_the_link_and_never_into_the_prompt() {
        // Arrange
        let (mut app, mut commands) = linked();

        // Act
        type_line(&mut app, "/model fast");
        type_line(&mut app, "/backend");
        type_line(&mut app, "/backend faux");

        // Assert
        assert_eq!(
            sent(&mut commands),
            vec![
                LinkCommand::SetModel("fast".to_owned()),
                LinkCommand::ListBackends,
                LinkCommand::SwitchBackend("faux".to_owned()),
            ]
        );
        assert!(!app.turn_running);
    }

    #[test]
    fn a_backend_switch_is_refused_while_a_turn_is_running() {
        // Arrange
        let (mut app, mut commands) = linked();
        type_line(&mut app, "q hi");
        sent(&mut commands);

        // Act
        type_line(&mut app, "/backend faux");

        // Assert
        assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
        let shown = wide_screen(&app);
        assert!(shown.contains("wait for the running turn"), "{shown}");
    }

    #[test]
    fn a_prompt_starting_with_a_path_is_sent_to_the_model() {
        // Arrange
        let (mut app, mut commands) = linked();

        // Act
        type_line(&mut app, "/usr/bin holds the tools");

        // Assert
        assert_eq!(
            sent(&mut commands),
            vec![LinkCommand::Send("/usr/bin holds the tools".to_owned())]
        );
    }

    #[test]
    fn a_model_command_without_an_alias_shows_usage_and_sends_nothing() {
        // Arrange
        let (mut app, mut commands) = linked();

        // Act
        type_line(&mut app, "/model");

        // Assert
        assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
        let shown = wide_screen(&app);
        assert!(shown.contains("/model needs a model alias"), "{shown}");
    }

    #[test]
    fn a_switched_model_shows_in_the_status_line_and_the_transcript() {
        // Arrange
        let mut app = App::new();
        app.handle_event(LinkEvent::SessionStarted(session("s-1")));
        let mut switched = session("s-1");
        switched.model = Some("claude-fast".to_owned());

        // Act
        app.handle_event(LinkEvent::ModelSwitched(switched));

        // Assert
        let shown = wide_screen(&app);
        assert!(shown.contains("faux | claude-fast"), "{shown}");
        assert!(shown.contains("model is now claude-fast"), "{shown}");
    }

    #[test]
    fn the_notice_after_a_backend_switch_says_the_transcript_is_not_carried_over() {
        // Arrange
        let mut app = App::new();
        app.handle_event(LinkEvent::UserMessage("old chat".to_owned()));
        let mut started = session("s-2");
        started.backend = BackendKind::Anthropic;

        // Act
        app.handle_event(LinkEvent::SessionStarted(started));
        app.handle_event(LinkEvent::Notice(
            crate::sessions::TRANSCRIPT_NOT_CARRIED.to_owned(),
        ));

        // Assert
        let shown = wide_screen(&app);
        assert!(!shown.contains("old chat"), "{shown}");
        assert!(
            shown.contains("the transcript is not carried over to the new session"),
            "{shown}"
        );
        assert!(shown.contains("anthropic | m-1"), "{shown}");
    }

    fn screen_at(app: &App, now: Instant) -> String {
        crate::test_support::screen(WIDTH, HEIGHT, |frame| app.render_at(frame, now))
    }

    fn only_response(commands: &mut UnboundedReceiver<LinkCommand>) -> PermissionDecision {
        match sent(commands).as_slice() {
            [LinkCommand::Respond(answer)] => answer.decision,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_paste_between_two_ctrl_c_presses_resets_the_confirmation() {
        // Arrange
        let mut app = App::new();
        let start = Instant::now();

        // Act
        app.handle_key_at(ctrl_c(), start);
        app.handle_paste("keep me");
        app.handle_key_at(ctrl_c(), start);

        // Assert
        assert!(!app.should_quit());
        assert_eq!(app.input.text(), "keep me");
    }

    #[test]
    fn the_title_fits_a_narrow_terminal_and_promises_only_what_is_true() {
        // Arrange
        let app = App::new();

        // Act
        let first_row = screen_at(&app, Instant::now())
            .lines()
            .next()
            .unwrap()
            .to_owned();

        // Assert
        assert!(first_row.contains(TITLE.trim()), "{first_row}");
    }

    #[test]
    fn the_quit_confirmation_shows_once_and_goes_when_the_window_closes() {
        // Arrange
        let mut app = App::new();
        let start = Instant::now();

        // Act
        app.handle_key_at(ctrl_c(), start);
        app.handle_key_at(ctrl_c(), start + QUIT_CONFIRM_WINDOW * 2);
        let inside = screen_at(&app, start + QUIT_CONFIRM_WINDOW * 2);
        let after = screen_at(&app, start + QUIT_CONFIRM_WINDOW * 4);

        // Assert
        assert!(inside.contains(QUIT_CONFIRM_NOTICE), "{inside}");
        assert!(!after.contains(QUIT_CONFIRM_NOTICE), "{after}");
    }

    #[test]
    fn the_confirmation_does_not_pile_up_in_the_transcript() {
        // Arrange
        let mut app = App::new();
        let start = Instant::now();

        // Act
        for step in 0..3 {
            app.handle_key_at(ctrl_c(), start + QUIT_CONFIRM_WINDOW * (step * 3));
        }

        // Assert
        let shown = screen_at(&app, start + QUIT_CONFIRM_WINDOW * 100);
        assert!(!shown.contains(QUIT_CONFIRM_NOTICE), "{shown}");
    }

    #[test]
    fn ctrl_c_in_a_busy_turn_tells_how_to_quit() {
        // Arrange
        let (mut app, _commands) = linked();
        type_line(&mut app, "go");
        let start = Instant::now();

        // Act
        app.handle_key_at(ctrl_c(), start);

        // Assert
        let shown = screen_at(&app, start);
        assert!(shown.contains(BUSY_HINT), "{shown}");
        assert!(!app.should_quit());
    }

    #[test]
    fn ctrl_d_during_a_turn_does_nothing_with_text_and_quits_when_empty() {
        // Arrange
        let (mut app, mut commands) = linked();
        type_line(&mut app, "go");
        sent(&mut commands);
        app.handle_key(press(KeyCode::Char('x')));

        // Act
        app.handle_key(ctrl_d());
        let with_text = app.should_quit();
        app.handle_key(press(KeyCode::Esc));
        app.handle_key(ctrl_d());

        // Assert
        assert!(!with_text);
        assert!(app.should_quit());
        assert!(app.turn_running);
        assert_eq!(sent(&mut commands), vec![]);
    }

    #[test]
    fn the_lost_daemon_hint_shows_on_a_narrow_terminal_and_ctrl_c_keeps_the_reason() {
        // Arrange
        let mut app = App::new();
        app.handle_event(LinkEvent::Disconnected("EOF".to_owned()));
        let now = Instant::now();

        // Act
        app.handle_key_at(ctrl_c(), now);

        // Assert
        let shown = screen_at(&app, now);
        assert!(shown.contains("Ctrl+D quits"), "{shown}");
        assert!(!shown.contains(QUIT_CONFIRM_NOTICE), "{shown}");
    }

    #[test]
    fn denying_a_prompt_with_ctrl_c_does_not_say_cancelling() {
        // Arrange
        let (mut app, _commands) = armed();
        app.handle_event(prompt("r-1"));
        let now = Instant::now();

        // Act
        app.handle_key_at(ctrl_c(), now);

        // Assert
        let shown = screen_at(&app, now);
        assert!(!shown.contains(BUSY_HINT), "{shown}");
    }

    #[test]
    fn esc_during_a_turn_clears_the_input_and_leaves_the_turn_running() {
        // Arrange
        let (mut app, mut commands) = linked();
        type_line(&mut app, "go");
        sent(&mut commands);
        app.handle_key(press(KeyCode::Char('x')));

        // Act
        app.handle_key(press(KeyCode::Esc));

        // Assert
        assert_eq!(app.input.text(), "");
        assert!(app.turn_running);
        assert!(!app.should_quit());
        assert_eq!(sent(&mut commands), vec![]);
    }

    #[test]
    fn ctrl_d_with_a_prompt_open_neither_quits_nor_answers() {
        // Arrange
        let (mut app, mut commands) = armed();
        app.handle_event(prompt("r-1"));

        // Act
        app.handle_key(ctrl_d());

        // Assert
        assert!(!app.should_quit());
        assert!(app.overlay.is_active());
        assert_eq!(sent(&mut commands), vec![]);
    }

    #[test]
    fn esc_in_a_prompt_denies_the_request() {
        // Arrange
        let (mut app, mut commands) = armed();
        app.handle_event(prompt("r-1"));

        // Act
        app.handle_key(press(KeyCode::Esc));

        // Assert
        assert_eq!(only_response(&mut commands), PermissionDecision::Deny);
    }

    #[test]
    fn a_lost_daemon_with_text_left_in_the_input_can_still_quit() {
        // Arrange
        let mut app = App::new();
        app.handle_key(press(KeyCode::Char('x')));
        app.handle_event(LinkEvent::Disconnected("EOF".to_owned()));

        // Act
        app.handle_key(ctrl_d());

        // Assert
        assert!(app.should_quit());
    }

    #[test]
    fn a_lost_daemon_can_quit_with_ctrl_c_twice_and_with_slash_quit() {
        // Arrange
        let lost = || {
            let mut app = App::new();
            app.handle_event(LinkEvent::Disconnected("EOF".to_owned()));
            app
        };
        let mut by_ctrl_c = lost();
        let mut by_command = lost();
        let start = Instant::now();

        // Act
        by_ctrl_c.handle_key_at(ctrl_c(), start);
        by_ctrl_c.handle_key_at(ctrl_c(), start);
        type_line(&mut by_command, "/quit");

        // Assert
        assert!(by_ctrl_c.should_quit());
        assert!(by_command.should_quit());
    }
}
