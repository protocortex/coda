// SPDX-License-Identifier: Apache-2.0

//! UI state, key handling and rendering.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use coda_config::tui::NotifyMode;
use coda_protocol::events::{Event, InterruptCause, TodoItem};
use coda_protocol::methods::{Capabilities, SessionDiffResult, SessionSummary};
use coda_protocol::types::{BackendKind, Mode, SessionId, SessionInfo, TurnId};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Paragraph};
use tokio::sync::mpsc::UnboundedSender;

use crate::activity::{self, Stage};
use crate::chat::Chat;
use crate::commands::{self, Command, CommandError, Resolved, UserCommands};
use crate::completion;
use crate::diffview::{self, DiffView};
use crate::input::{Input, Stashed};
use crate::notify;
use crate::paste;
use crate::permission::{Overlay, PermissionPrompt, sanitize_line};
use crate::picker::{self, Picker};
use crate::sessions::LinkCommand;
use crate::status::{Status, mode_name};
use crate::todos;
use crate::work::WorkLog;

const NOTIFY_TITLE: &str = "coda";
const NOTIFY_APPROVAL: &str = "needs your approval";
const NOTIFY_QUESTION: &str = "the model asks you a question";
const NOTIFY_FAILED: &str = "turn failed";
const NOTIFY_FINISHED: &str = "turn finished";
const NOTIFY_STOPPED: &str = "turn stopped";
const NOTIFY_CRASHED: &str = "backend crashed";
const NOTIFY_LOST: &str = "connection lost";
/// Longest tool name put in a notification, in characters.
const NOTIFY_TOOL_CHARS: usize = 40;
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
const NO_PASTE_FOLDER: &str = "no session folder yet";
const NO_COMPLETION_FOLDER: &str = "no session folder yet to complete paths in";
const NO_FILE_MATCHES: &str = "no file matches";
const MORE_CHOICES: &str = ", ...";
const CLEARED: &str = "prompt cleared; Ctrl+S brings it back";
const NOTHING_STASHED: &str = "nothing is stashed";
const STASHED: &str = "prompt stashed; press Ctrl+S on an empty prompt to bring it back";
const ALREADY_PLANNING: &str = "the session is already in plan mode";
const NO_LIMITS: &str = "no plan limits reported yet";
const NO_DIFF: &str = "this daemon cannot show a diff";
const NO_CHANGES: &str = "the last turn changed no files";
const NOT_PLANNING: &str =
    "the session is not in plan mode, so there is no plan to carry out; use /plan first";
const NO_PLAN: &str = "there is no plan to save yet; ask the agent for one in plan mode first";
const MODE_CHANGE_BUSY: &str = "wait for the running turn to finish before changing the mode";
const MODE_CHANGE_UNSUPPORTED: &str =
    "this daemon cannot change the mode of a session; restart it with `coda daemon restart`";
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
    /// The daemon changed the mode of the session the UI follows. `started_turn`
    /// is true when a message followed the change, as `/implement` sends one.
    ModeChanged {
        info: SessionInfo,
        started_turn: bool,
    },
    /// The user's own message, live or replayed from the stored history.
    UserMessage(String),
    /// The daemon's sessions, answering `/sessions`.
    Sessions(Vec<SessionSummary>),
    /// The files the turn that just ended changed.
    TurnChanges {
        session: SessionId,
        changes: SessionDiffResult,
    },
    /// The diff of the last turn, answering `/diff`.
    Diff {
        session: SessionId,
        diff: SessionDiffResult,
    },
    /// Something the user should read, such as a failed request.
    Notice(String),
    /// What the daemon said it can do when the link connected.
    Capabilities(Capabilities),
    /// The model's todo list changed; this is the whole list now.
    Todos(Vec<TodoItem>),
    /// The daemon has no enabled provider.
    SetupNeeded,
    /// The process was asked to stop, for example by a signal.
    Quit,
}

/// Whether the terminal has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Gained,
    Lost,
}

#[derive(Debug)]
pub struct App {
    connection: ConnectionState,
    chat: Chat,
    input: Input,
    /// The session picker, while it is open.
    picker: Option<Picker>,
    /// The diff of the last turn, while it is open.
    diff_view: Option<DiffView>,
    /// The session the UI follows, once one has started.
    session: Option<SessionId>,
    /// The folder the session works in, once a session has started.
    cwd: Option<String>,
    status: Status,
    overlay: Overlay,
    link: Option<UnboundedSender<LinkCommand>>,
    turn_running: bool,
    /// The text of the plan-mode turn being streamed, by its turn.
    plan_draft: Option<(TurnId, String)>,
    /// What the agent last said in a plan-mode turn that ended well.
    plan: Option<String>,
    work: WorkLog,
    /// The model's todo list, drawn above the prompt.
    todos: Vec<TodoItem>,
    /// When the user asked to stop the turn or last forced it, and whether that was a force.
    stop: Option<(Instant, bool)>,
    /// What the daemon said it can do; unknown until the link reports it.
    capabilities: Option<Capabilities>,
    /// Whether the terminal has focus. Assumed until the terminal says it lost it.
    focus: Focus,
    /// How to tell the user, while out of focus, that the UI needs them.
    notify: NotifyMode,
    /// A title and text waiting to be written to the terminal.
    notification: Option<(String, String)>,
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
            picker: None,
            diff_view: None,
            session: None,
            cwd: None,
            status: Status::new(),
            overlay: Overlay::default(),
            link: None,
            turn_running: false,
            plan_draft: None,
            plan: None,
            work: WorkLog::new(),
            todos: Vec::new(),
            stop: None,
            capabilities: None,
            focus: Focus::Gained,
            notify: NotifyMode::Off,
            notification: None,
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
        self.handle_event_at(event, Instant::now());
    }

    fn handle_event_at(&mut self, event: LinkEvent, now: Instant) {
        match event {
            LinkEvent::Connected => self.connection = ConnectionState::Connected,
            LinkEvent::Failed(reason) => self.connection = ConnectionState::Failed(reason),
            LinkEvent::Disconnected(reason) => {
                self.connection = ConnectionState::Disconnected(reason);
                self.overlay = Overlay::default();
                if self.turn_running {
                    self.notify_user(NOTIFY_TITLE, NOTIFY_LOST.to_owned());
                }
                self.turn_running = false;
                self.work.reset();
            }
            LinkEvent::Session(event) => self.apply_session_event(&event, now),
            LinkEvent::Permission(prompt) => {
                let body = if prompt.options.is_empty() {
                    let tool: String = prompt.tool.chars().take(NOTIFY_TOOL_CHARS).collect();
                    format!("{tool} {NOTIFY_APPROVAL}")
                } else {
                    NOTIFY_QUESTION.to_owned()
                };
                if self.overlay.push(prompt, Instant::now()) {
                    self.notify_user(NOTIFY_TITLE, body);
                }
            }
            LinkEvent::SessionStarted(info) => {
                self.chat.clear();
                self.todos.clear();
                self.status.set_session(info.backend, info.model);
                self.status.set_mode(info.mode);
                self.plan_draft = None;
                self.plan = None;
                self.cwd = Some(info.cwd);
                self.session = Some(info.id);
                self.diff_view = None;
                self.overlay = Overlay::default();
                self.turn_running = false;
                self.work.reset();
            }
            LinkEvent::ModelSwitched(info) => {
                self.chat.push_notice(format!(
                    "model is now {}",
                    info.model.as_deref().unwrap_or("the default")
                ));
                self.status.switch_model(info.model);
            }
            LinkEvent::ModeChanged { info, started_turn } => {
                self.chat
                    .push_notice(format!("mode is now {}", mode_name(info.mode)));
                self.status.set_mode(info.mode);
                if started_turn {
                    self.work.begin(Instant::now());
                    self.stop = None;
                    self.turn_running = true;
                }
            }
            LinkEvent::UserMessage(text) => self.chat.push_user(text),
            LinkEvent::Sessions(sessions) => self.open_picker(&sessions),
            LinkEvent::TurnChanges { session, changes } if self.is_current(&session) => {
                self.chat.push_notice(changes_line(&changes));
            }
            LinkEvent::Diff { session, diff } if self.is_current(&session) => {
                self.open_diff(&diff);
            }
            LinkEvent::TurnChanges { .. } | LinkEvent::Diff { .. } => {}
            LinkEvent::Notice(text) => self.chat.push_notice(text),
            LinkEvent::Capabilities(capabilities) => self.capabilities = Some(capabilities),
            LinkEvent::Todos(items) => self.todos = items,
            LinkEvent::SetupNeeded => self.show_setup_guidance(),
            LinkEvent::Quit => self.quit = true,
        }
    }

    /// Keeps what the agent says in plan mode, so `/implement` and `/plan save`
    /// can use the last finished plan.
    fn track_plan(&mut self, event: &Event) {
        if self.status.mode() != Mode::Plan {
            return;
        }
        match event {
            Event::StreamDelta { turn_id, text, .. } => match &mut self.plan_draft {
                Some((turn, draft)) if turn == turn_id => draft.push_str(text),
                _ => self.plan_draft = Some((turn_id.clone(), text.clone())),
            },
            Event::Result {
                turn_id,
                is_error: false,
                ..
            } => {
                if let Some((turn, draft)) = self.plan_draft.take()
                    && &turn == turn_id
                    && !draft.trim().is_empty()
                {
                    self.plan = Some(draft);
                    self.status.set_plan_ready(true);
                }
            }
            _ => {}
        }
    }

    fn apply_session_event(&mut self, event: &Event, now: Instant) {
        self.track_plan(event);
        match event {
            Event::Approve { request_id, .. }
            | Event::Deny { request_id, .. }
            | Event::Timeout { request_id, .. } => self.overlay.resolve(request_id),
            Event::Result { is_error, .. } => {
                if self.turn_running {
                    let body = if *is_error {
                        NOTIFY_FAILED
                    } else {
                        NOTIFY_FINISHED
                    };
                    self.notify_user(NOTIFY_TITLE, body.to_owned());
                    if self.capabilities.is_some_and(|known| known.diff) {
                        self.send(LinkCommand::TurnChanges);
                    }
                }
                self.turn_running = false;
            }
            Event::TurnInterrupted { cause, .. } => {
                if self.turn_running {
                    self.notify_user(NOTIFY_TITLE, NOTIFY_STOPPED.to_owned());
                }
                self.turn_running = false;
                if *cause == Some(InterruptCause::BackendLost) {
                    self.overlay.backend_gone();
                }
            }
            // A restart keeps the same turn going, so only a final crash ends it.
            Event::BackendCrashed { will_restart, .. } => {
                if self.turn_running && !*will_restart {
                    self.notify_user(NOTIFY_TITLE, NOTIFY_CRASHED.to_owned());
                }
                self.turn_running &= *will_restart;
                self.overlay.backend_gone();
            }
            _ => {}
        }
        self.chat.apply(event);
        self.status.apply(event);
        if let Some(fold) = self.work.apply(event, now) {
            self.chat.push_fold(fold);
        }
    }

    /// The live line, when a tool runs or a prompt waits for an answer.
    fn work_line(&self, now: Instant) -> Option<String> {
        self.work.live(now, self.overlay.is_active())
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
        if ctrl && key.code == KeyCode::Char('c') && self.diff_view.take().is_some() {
            return;
        }
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
        } else if let Some(view) = self.diff_view.as_mut() {
            if view.handle_key(key) == diffview::Action::Close {
                self.diff_view = None;
            }
        } else if ctrl && key.code == KeyCode::Char('k') {
            self.toggle_picker();
        } else if let Some(open) = self.picker.as_mut() {
            match open.handle_key(key) {
                picker::Action::None => {}
                picker::Action::Close => self.picker = None,
                picker::Action::Resume(id) => {
                    self.picker = None;
                    self.send(LinkCommand::Resume(id));
                }
            }
        } else if ctrl && key.code == KeyCode::Char('d') {
            let lost = matches!(self.connection, ConnectionState::Disconnected(_));
            if lost || self.input.text().is_empty() {
                self.quit = true;
            }
        } else if ctrl && key.code == KeyCode::Char('o') {
            self.chat.toggle_fold();
        } else if ctrl && key.code == KeyCode::Char('e') {
            self.chat.toggle_error();
        } else if ctrl && key.code == KeyCode::Char('s') {
            self.stash_prompt();
        } else if key.code == KeyCode::Esc {
            if self.input.discard() {
                self.chat.push_notice(CLEARED.to_owned());
            }
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
            self.stop_turn(now);
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

    /// Asks the daemon to cancel the turn. Once a stop has gone unanswered for
    /// [`activity::STOP_TIMEOUT`], the next press forces it.
    fn stop_turn(&mut self, now: Instant) {
        let offered = self.can_force()
            && self.stop.is_some_and(|(asked, forced)| {
                activity::stage(asked, forced, now) == Stage::NotResponding
            });
        if offered {
            self.stop = Some((now, true));
            self.send(LinkCommand::ForceCancel);
        } else {
            self.stop.get_or_insert((now, false));
            self.send(LinkCommand::Cancel);
        }
    }

    /// Sets how the user is told, while the terminal is out of focus, that a turn
    /// ended or an approval waits.
    pub fn set_notify(&mut self, mode: NotifyMode) {
        self.notify = mode;
    }

    /// Records that the terminal gained or lost focus. Gaining it drops a
    /// notification that was not written yet, since the user is looking.
    pub fn set_focus(&mut self, gained: bool) {
        self.focus = if gained { Focus::Gained } else { Focus::Lost };
        if gained {
            self.notification = None;
        }
    }

    /// The terminal sequence for the notification waiting to be written, once.
    pub fn take_notification(&mut self) -> Option<String> {
        let (title, body) = self.notification.take()?;
        notify::sequence(self.notify, &title, &body)
    }

    fn notify_user(&mut self, title: &str, body: String) {
        if self.focus == Focus::Lost {
            self.notification = Some((title.to_owned(), body));
        }
    }

    /// Whether a forced stop is worth offering. Assumed until the daemon says it is not.
    fn can_force(&self) -> bool {
        self.capabilities.is_none_or(|known| known.force_cancel)
    }

    /// What the status line says about the running turn.
    fn activity(&self, now: Instant) -> Option<String> {
        if !self.turn_running {
            return None;
        }
        let began = self.work.began().unwrap_or(now);
        Some(activity::status_text(
            began,
            self.stop,
            self.can_force(),
            now,
        ))
    }

    fn handle_prompt_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::PageUp => self.chat.scroll_up(PAGE_ROWS),
            KeyCode::PageDown => self.chat.scroll_down(PAGE_ROWS),
            KeyCode::Tab => self.complete_path(),
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
        // A send while a turn runs is refused by the daemon, so it must not reset the turn's log.
        if !self.turn_running {
            self.work.begin(Instant::now());
            self.stop = None;
        }
        self.turn_running = true;
        if !self.send(LinkCommand::Send(text.to_owned())) {
            self.input.restore(typed);
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
            Command::Diff if self.capabilities.is_some_and(|known| !known.diff) => {
                self.chat.push_notice(NO_DIFF.to_owned());
                return;
            }
            Command::Diff => LinkCommand::Diff,
            Command::New => LinkCommand::New,
            Command::Resume(id) => LinkCommand::Resume(id),
            Command::Model(alias) => LinkCommand::SetModel(alias),
            Command::Backend(None) => LinkCommand::ListBackends,
            Command::Backend(Some(_)) if self.turn_running => {
                self.chat.push_notice(BACKEND_SWITCH_BUSY.to_owned());
                return;
            }
            Command::Backend(Some(name)) => LinkCommand::SwitchBackend(name),
            Command::Plan if self.status.mode() == Mode::Plan => {
                self.chat.push_notice(ALREADY_PLANNING.to_owned());
                return;
            }
            Command::Plan => return self.change_mode(Mode::Plan, None),
            Command::PlanSave(path) => {
                self.save_plan(&path);
                return;
            }
            Command::Implement if self.turn_running => {
                self.chat.push_notice(MODE_CHANGE_BUSY.to_owned());
                return;
            }
            Command::Implement if self.status.mode() != Mode::Plan => {
                self.chat.push_notice(NOT_PLANNING.to_owned());
                return;
            }
            Command::Implement => {
                let prompt = commands::implement_prompt(self.plan.as_deref());
                return self.change_mode(Mode::Default, Some(prompt));
            }
            Command::Review(target) => {
                let typed = format!("/review {target}");
                self.send_message(&commands::review_prompt(&target), typed.trim_end());
                return;
            }
            Command::Help => {
                self.show_help();
                return;
            }
            Command::Limits => {
                let lines = self.status.limit_lines();
                if lines.is_empty() {
                    self.chat.push_notice(NO_LIMITS.to_owned());
                }
                for line in lines {
                    self.chat.push_notice(line);
                }
                return;
            }
            Command::Quit => {
                self.quit = true;
                return;
            }
        };
        self.send(link_command);
    }

    fn save_plan(&mut self, path: &str) {
        let Some(plan) = &self.plan else {
            self.chat.push_notice(NO_PLAN.to_owned());
            return;
        };
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .and_then(|mut file| writeln!(file, "{plan}"));
        let notice = match written {
            Ok(()) => format!("plan written to {path}"),
            Err(error) => format!("could not save the plan to {path}: {error}"),
        };
        self.chat.push_notice(sanitize_line(&notice));
    }

    fn change_mode(&mut self, mode: Mode, then: Option<String>) {
        if self.turn_running {
            self.chat.push_notice(MODE_CHANGE_BUSY.to_owned());
        } else if self.capabilities.is_some_and(|known| !known.set_mode) {
            self.chat.push_notice(MODE_CHANGE_UNSUPPORTED.to_owned());
        } else {
            self.send(LinkCommand::SetMode { mode, then });
        }
    }

    /// Opens the picker on `sessions`, or says there are none.
    fn open_picker(&mut self, sessions: &[SessionSummary]) {
        if sessions.is_empty() {
            self.chat.push_notice(NO_SESSIONS.to_owned());
        } else {
            self.picker = Some(Picker::new(sessions, picker::now_ms()));
        }
    }

    /// Whether an answer about `session` belongs to the session the UI follows.
    fn is_current(&self, session: &SessionId) -> bool {
        self.session
            .as_ref()
            .is_none_or(|current| current == session)
    }

    /// Opens the diff view, or says the turn changed nothing.
    fn open_diff(&mut self, diff: &SessionDiffResult) {
        if diff.files.is_empty() {
            self.chat.push_notice(NO_CHANGES.to_owned());
        } else {
            self.diff_view = Some(DiffView::new(diff));
        }
    }

    /// Closes the picker, or asks the daemon for the sessions to open it on.
    fn toggle_picker(&mut self) {
        if self.picker.take().is_none() {
            self.send(LinkCommand::ListSessions);
        }
    }

    pub fn handle_paste(&mut self, pasted: &str) {
        if self.picker.is_some() || self.diff_view.is_some() {
            return;
        }
        self.idle_ctrl_c_at = None;
        self.hint = None;
        let large = pasted.len() >= paste::LARGE_PASTE_BYTES;
        let cleaned = if large {
            paste::clean_for_file(pasted)
        } else {
            paste::clean(pasted)
        };
        if cleaned.invisible > 0 {
            self.chat
                .push_notice(paste::invisible_warning(cleaned.invisible));
        }
        if large {
            self.keep_large_paste(&cleaned.text, pasted);
        } else {
            self.input.insert(&cleaned.text);
        }
    }

    /// Writes a large paste to a file under the session's folder and puts a
    /// pointer to it in the prompt, so the prompt stays small and the agent reads the file.
    fn keep_large_paste(&mut self, text: &str, pasted: &str) {
        let saved = match &self.cwd {
            Some(folder) => paste::save(Path::new(folder), text).map_err(|e| e.to_string()),
            None => Err(NO_PASTE_FOLDER.to_owned()),
        };
        match saved {
            Ok(relative) => self.input.insert(&paste::reference(&relative, text.len())),
            Err(why) => {
                self.chat.push_notice(format!(
                    "could not keep the large paste in a file ({why}); it stays in the prompt"
                ));
                self.input.insert(&paste::clean(pasted).text);
            }
        }
    }

    /// Completes the `@path` word before the cursor against the session's folder.
    fn complete_path(&mut self) {
        let Some((start, word)) = self.input.token_at_cursor(completion::MARKER) else {
            return;
        };
        let word = word.to_owned();
        let Some(folder) = self.cwd.clone() else {
            self.chat.push_notice(NO_COMPLETION_FOLDER.to_owned());
            return;
        };
        let Some(found) = completion::complete(Path::new(&folder), &word) else {
            self.chat
                .push_notice(sanitize_line(&format!("{NO_FILE_MATCHES} {word}")));
            return;
        };
        self.input.replace_to_cursor(start, &found.replacement);
        if !found.choices.is_empty() {
            let more = if found.total > found.choices.len() {
                MORE_CHOICES
            } else {
                ""
            };
            let list = format!(
                "{} matches: {}{more}",
                found.total,
                found.choices.join(", ")
            );
            self.chat.push_notice(sanitize_line(&list));
        }
    }

    fn stash_prompt(&mut self) {
        match self.input.stash_toggle() {
            Stashed::Saved => self.chat.push_notice(STASHED.to_owned()),
            Stashed::Nothing => self.chat.push_notice(NOTHING_STASHED.to_owned()),
            Stashed::Restored => {}
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        self.render_at(frame, Instant::now());
    }

    fn render_at(&self, frame: &mut Frame, now: Instant) {
        let live = self.work_line(now);
        let todo_lines = todos::lines(&self.todos);
        let todo_rows = u16::try_from(todo_lines.len()).unwrap_or(u16::MAX);
        let [body, todo_row, live_row, input, connection, status] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(todo_rows),
            Constraint::Length(u16::from(live.is_some())),
            Constraint::Length(self.input.rows(frame.area().width)),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        let block = Block::bordered().title(TITLE);
        let transcript = block.inner(body);
        frame.render_widget(block, body);
        self.chat.render(frame, transcript);
        if !todo_lines.is_empty() {
            frame.render_widget(Paragraph::new(todo_lines.join("\n")), todo_row);
        }
        if let Some(live) = live {
            frame.render_widget(Paragraph::new(live), live_row);
        }
        self.input.render(
            frame,
            input,
            !self.overlay.is_active() && self.picker.is_none(),
        );
        frame.render_widget(Paragraph::new(self.connection_text(now)), connection);
        let activity = self.activity(now);
        self.status.render_with(frame, status, activity.as_deref());
        self.overlay.render(frame, frame.area());
        if let Some(open) = self.picker.as_ref().filter(|_| !self.overlay.is_active()) {
            open.render(frame, frame.area());
        }
        if let Some(view) = self
            .diff_view
            .as_ref()
            .filter(|_| !self.overlay.is_active())
        {
            view.render(frame, frame.area());
        }
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

/// The line that follows a turn that changed files: how many, and how much.
fn changes_line(changes: &SessionDiffResult) -> String {
    let count = changes.files.len();
    let added: u64 = changes.files.iter().map(|file| file.added).sum();
    let deleted: u64 = changes.files.iter().map(|file| file.deleted).sum();
    let more = if changes.truncated { "+" } else { "" };
    let noun = if count == 1 && !changes.truncated {
        "file"
    } else {
        "files"
    };
    format!("changed {count}{more} {noun} +{added} -{deleted} (/diff)")
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

    use coda_protocol::events::ToolCallStatus;
    use coda_protocol::methods::{
        Capabilities, DiffFile, SessionAttention, SessionStatus, SessionSummary,
    };
    use coda_protocol::types::{
        Mode, PermissionDecision, RequestId, SessionId, TokenUsage, TurnId,
    };
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
            context: None,
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
            options: Vec::new(),
            reason: None,
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
                choice: None,
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

    fn listed(id: &str, title: &str, attention: SessionAttention) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(id),
            backend: BackendKind::Faux,
            cwd: "/work".to_owned(),
            created_at: 1,
            updated_at: 2,
            status: SessionStatus::Running,
            attention,
            title: Some(title.to_owned()),
            pull_requests: Vec::new(),
        }
    }

    fn tall_screen(app: &App) -> String {
        crate::test_support::screen(120, 20, |frame| app.render(frame))
    }

    #[test]
    fn a_replayed_user_message_stays_in_the_transcript_under_the_session_picker() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_event(LinkEvent::UserMessage("hello again".to_owned()));
        let before = tall_screen(&app);
        app.handle_event(LinkEvent::Sessions(vec![listed(
            "s-7",
            "fix it",
            SessionAttention::None,
        )]));

        // Assert
        assert!(before.contains("hello again"), "{before}");
        let shown = tall_screen(&app);
        assert!(shown.contains("Sessions"), "{shown}");
        assert!(shown.contains("working"), "{shown}");
        assert!(shown.contains("fix it #s-7"), "{shown}");
    }

    #[test]
    fn a_session_list_with_nothing_in_it_says_so_and_opens_no_picker() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_event(LinkEvent::Sessions(Vec::new()));

        // Assert
        let shown = tall_screen(&app);
        assert!(shown.contains("the daemon has no sessions"), "{shown}");
        assert!(!shown.contains("Esc closes"), "{shown}");
    }

    #[test]
    fn control_k_asks_for_the_sessions_and_closes_the_picker_when_it_is_open() {
        // Arrange
        let (mut app, mut commands) = linked();

        // Act
        app.handle_key(key_with(KeyCode::Char('k'), KeyModifiers::CONTROL));
        let asked = sent(&mut commands);
        app.handle_event(LinkEvent::Sessions(vec![listed(
            "s-7",
            "fix it",
            SessionAttention::None,
        )]));
        let open = tall_screen(&app);
        app.handle_key(key_with(KeyCode::Char('k'), KeyModifiers::CONTROL));
        let closed = tall_screen(&app);

        // Assert
        assert_eq!(asked, vec![LinkCommand::ListSessions]);
        assert!(open.contains("Esc closes"), "{open}");
        assert!(!closed.contains("Esc closes"), "{closed}");
        assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
    }

    #[test]
    fn keys_go_to_the_open_picker_and_enter_resumes_the_chosen_session() {
        // Arrange
        let (mut app, mut commands) = linked();
        app.handle_event(LinkEvent::Sessions(vec![
            listed("s-1", "alpha", SessionAttention::None),
            listed("s-2", "beta", SessionAttention::Approval),
        ]));

        // Act
        type_line_keys(&mut app, "alp");
        let narrowed = tall_screen(&app);
        app.handle_key(press(KeyCode::Enter));

        // Assert
        assert!(narrowed.contains("filter: alp"), "{narrowed}");
        assert!(!narrowed.contains("beta"), "{narrowed}");
        assert_eq!(
            sent(&mut commands),
            vec![LinkCommand::Resume(SessionId::new("s-1"))]
        );
        assert_eq!(app.input.text(), "");
        assert!(!tall_screen(&app).contains("Esc closes"));
    }

    #[test]
    fn a_paste_while_the_picker_is_open_does_not_reach_the_hidden_prompt() {
        // Arrange
        let (mut app, _commands) = linked();
        app.handle_event(LinkEvent::Sessions(vec![listed(
            "s-1",
            "alpha",
            SessionAttention::None,
        )]));

        // Act
        app.handle_paste("secret text");

        // Assert
        assert_eq!(app.input.text(), "");
    }

    #[test]
    fn escape_closes_the_picker_and_keeps_what_was_typed_in_the_prompt() {
        // Arrange
        let (mut app, mut commands) = linked();
        app.handle_paste("half a thought");
        app.handle_event(LinkEvent::Sessions(vec![listed(
            "s-1",
            "alpha",
            SessionAttention::None,
        )]));

        // Act
        app.handle_key(press(KeyCode::Esc));

        // Assert
        assert!(!tall_screen(&app).contains("Esc closes"));
        assert_eq!(app.input.text(), "half a thought");
        assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
    }

    #[test]
    fn a_permission_prompt_takes_the_keys_before_the_picker() {
        // Arrange
        let (mut app, mut commands) = armed();
        app.handle_event(LinkEvent::Sessions(vec![listed(
            "s-1",
            "alpha",
            SessionAttention::None,
        )]));
        app.handle_event(prompt("r-1"));
        tall_screen(&app);

        // Act
        app.handle_key(press(KeyCode::Char('y')));

        // Assert
        let sent = sent(&mut commands);
        assert!(
            matches!(sent.as_slice(), [LinkCommand::Respond(_)]),
            "{sent:?}"
        );
    }

    fn changes(files: &[(&str, u64, u64)], patch: &str, truncated: bool) -> SessionDiffResult {
        SessionDiffResult {
            turn_id: TurnId::new("t-1"),
            files: files
                .iter()
                .map(|(path, added, deleted)| DiffFile {
                    path: (*path).to_owned(),
                    added: *added,
                    deleted: *deleted,
                    binary: false,
                })
                .collect(),
            patch: patch.to_owned(),
            truncated,
        }
    }

    fn turn_changes_event(changes: SessionDiffResult) -> LinkEvent {
        LinkEvent::TurnChanges {
            session: SessionId::new("s-1"),
            changes,
        }
    }

    fn diff_event(diff: SessionDiffResult) -> LinkEvent {
        LinkEvent::Diff {
            session: SessionId::new("s-1"),
            diff,
        }
    }

    fn can_diff() -> Capabilities {
        Capabilities {
            diff: true,
            ..Capabilities::default()
        }
    }

    #[test]
    fn a_turn_that_ends_asks_what_it_changed_when_the_daemon_can_say() {
        // Arrange
        let (mut app, mut commands) = armed();
        app.handle_event(LinkEvent::Capabilities(can_diff()));
        type_line(&mut app, "go");
        sent(&mut commands);

        // Act
        app.handle_event(result_event(false));

        // Assert
        assert_eq!(sent(&mut commands), vec![LinkCommand::TurnChanges]);
    }

    #[test]
    fn limits_lists_each_window_the_provider_reported_or_says_there_are_none() {
        // Arrange
        let (mut app, _commands) = linked();
        type_line(&mut app, "/limits");
        let empty = tall_screen(&app);
        let resets_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3_600;
        app.handle_event(LinkEvent::Session(Event::RateLimit {
            session_id: SessionId::new("s-1"),
            limit: coda_protocol::events::RateLimit {
                status: "allowed".to_owned(),
                window_type: None,
                resets_at: None,
                windows: vec![coda_protocol::events::RateLimitWindow {
                    name: "seven_day".to_owned(),
                    utilization_bps: 2_500,
                    resets_at: Some(resets_at),
                }],
                is_using_overage: false,
                overage_status: None,
                overage_resets_at: None,
                overage_disabled_reason: None,
            },
        }));

        // Act
        type_line(&mut app, "/limits");
        let shown = tall_screen(&app);

        // Assert
        assert!(empty.contains(NO_LIMITS), "{empty}");
        assert!(shown.contains("7d 75% left, resets in "), "{shown}");
    }

    #[test]
    fn a_turn_that_ends_asks_nothing_of_a_daemon_that_cannot_diff_or_has_not_said() {
        // Arrange
        let (mut old, mut old_commands) = armed();
        old.handle_event(LinkEvent::Capabilities(Capabilities::default()));
        type_line(&mut old, "go");
        let (mut unknown, mut unknown_commands) = armed();
        type_line(&mut unknown, "go");
        sent(&mut old_commands);
        sent(&mut unknown_commands);

        // Act
        old.handle_event(result_event(false));
        unknown.handle_event(result_event(false));

        // Assert
        assert_eq!(sent(&mut old_commands), Vec::<LinkCommand>::new());
        assert_eq!(sent(&mut unknown_commands), Vec::<LinkCommand>::new());
    }

    #[test]
    fn a_replayed_turn_end_asks_nothing() {
        // Arrange
        let (mut app, mut commands) = armed();
        app.handle_event(LinkEvent::Capabilities(can_diff()));

        // Act
        app.handle_event(result_event(false));

        // Assert
        assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
    }

    #[test]
    fn the_files_a_turn_changed_are_counted_in_a_line_that_points_to_diff() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_event(turn_changes_event(changes(
            &[("a.rs", 10, 2), ("b.rs", 3, 1)],
            "",
            false,
        )));

        // Assert
        let shown = tall_screen(&app);
        assert!(shown.contains("changed 2 files +13 -3 (/diff)"), "{shown}");
    }

    #[test]
    fn one_file_reads_as_a_file_and_a_cut_list_as_at_least_that_many() {
        // Act
        let one = changes_line(&changes(&[("a.rs", 1, 0)], "", false));
        let cut = changes_line(&changes(&[("a.rs", 1, 0), ("b.rs", 1, 0)], "", true));

        // Assert
        assert_eq!(one, "changed 1 file +1 -0 (/diff)");
        assert_eq!(cut, "changed 2+ files +2 -0 (/diff)");
    }

    #[test]
    fn diff_asks_the_daemon_and_the_answer_opens_the_view_over_the_transcript() {
        // Arrange
        let (mut app, mut commands) = linked();
        app.handle_event(LinkEvent::Capabilities(can_diff()));
        app.handle_event(LinkEvent::UserMessage("hello again".to_owned()));

        // Act
        type_line(&mut app, "/diff");
        let asked = sent(&mut commands);
        app.handle_event(diff_event(changes(
            &[("a.txt", 1, 1)],
            "diff --git a/a.txt b/a.txt\n-one\n+two\n",
            false,
        )));
        let shown = tall_screen(&app);

        // Assert
        assert_eq!(asked, vec![LinkCommand::Diff]);
        assert!(shown.contains("Turn changes: 1 file +1 -1"), "{shown}");
        assert!(shown.contains("+two"), "{shown}");
    }

    #[test]
    fn a_diff_of_a_turn_that_changed_nothing_says_so_and_opens_no_view() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_event(diff_event(changes(&[], "", false)));

        // Assert
        let shown = tall_screen(&app);
        assert!(shown.contains("the last turn changed no files"), "{shown}");
        assert!(!shown.contains("Turn changes"), "{shown}");
    }

    #[test]
    fn diff_against_a_daemon_that_cannot_says_so_and_sends_nothing() {
        // Arrange
        let (mut app, mut commands) = linked();
        app.handle_event(LinkEvent::Capabilities(Capabilities::default()));

        // Act
        type_line(&mut app, "/diff");

        // Assert
        assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
        assert!(tall_screen(&app).contains("cannot show a diff"));
    }

    #[test]
    fn keys_go_to_the_open_diff_and_escape_closes_it_without_touching_the_prompt() {
        // Arrange
        let (mut app, mut commands) = linked();
        app.handle_paste("half a thought");
        app.handle_event(diff_event(changes(&[("a.txt", 1, 0)], "+x\n", false)));

        // Act
        app.handle_key(press(KeyCode::Char('j')));
        app.handle_paste("pasted into the void");
        let open = tall_screen(&app);
        app.handle_key(press(KeyCode::Esc));
        let closed = tall_screen(&app);

        // Assert
        assert!(open.contains("Turn changes"), "{open}");
        assert!(!closed.contains("Turn changes"), "{closed}");
        assert_eq!(app.input.text(), "half a thought");
        assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
    }

    #[test]
    fn an_answer_about_another_session_is_dropped_and_a_new_session_closes_the_view() {
        // Arrange
        let mut app = App::new();
        app.handle_event(LinkEvent::SessionStarted(session("s-2")));

        // Act
        app.handle_event(turn_changes_event(changes(&[("a.rs", 1, 0)], "", false)));
        app.handle_event(diff_event(changes(&[("a.rs", 1, 0)], "+x\n", false)));
        let stale = tall_screen(&app);
        app.handle_event(LinkEvent::Diff {
            session: SessionId::new("s-2"),
            diff: changes(&[("a.rs", 1, 0)], "+x\n", false),
        });
        let open = tall_screen(&app);
        app.handle_event(LinkEvent::SessionStarted(session("s-3")));
        let closed = tall_screen(&app);

        // Assert
        assert!(!stale.contains("changed 1 file"), "{stale}");
        assert!(!stale.contains("Turn changes"), "{stale}");
        assert!(open.contains("Turn changes"), "{open}");
        assert!(!closed.contains("Turn changes"), "{closed}");
    }

    #[test]
    fn control_c_closes_the_open_diff_instead_of_arming_the_quit() {
        // Arrange
        let (mut app, _commands) = linked();
        app.handle_event(diff_event(changes(&[("a.txt", 1, 0)], "+x\n", false)));

        // Act
        app.handle_key(key_with(KeyCode::Char('c'), KeyModifiers::CONTROL));

        // Assert
        assert!(!tall_screen(&app).contains("Turn changes"));
        assert!(!app.should_quit());
    }

    fn type_line_keys(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle_key(press(KeyCode::Char(ch)));
        }
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

    fn backend_lost() -> LinkEvent {
        LinkEvent::Session(Event::TurnInterrupted {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            cause: Some(InterruptCause::BackendLost),
        })
    }

    #[test]
    fn a_cancelled_turn_does_not_claim_the_backend_process_is_gone() {
        // Arrange
        let (mut app, _commands) = armed();
        app.handle_event(prompt("r-1"));

        // Act
        app.handle_event(turn_ended());
        let shown = wide_screen(&app);

        // Assert
        assert!(!shown.contains("backend process is gone"), "{shown}");
    }

    fn result_event(is_error: bool) -> LinkEvent {
        LinkEvent::Session(Event::Result {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            is_error,
        })
    }

    /// A linked app that sends this notification mode, with a turn running.
    fn notifying(mode: NotifyMode) -> App {
        let (mut app, _commands) = armed();
        app.set_notify(mode);
        type_line(&mut app, "go");
        app
    }

    #[test]
    fn a_turn_that_ends_while_the_terminal_is_out_of_focus_rings_the_bell_once() {
        // Arrange
        let mut app = notifying(NotifyMode::Bell);
        app.set_focus(false);

        // Act
        app.handle_event(result_event(false));
        let first = app.take_notification();
        let second = app.take_notification();

        // Assert
        assert_eq!(first.as_deref(), Some("\u{7}"));
        assert_eq!(second, None);
    }

    #[test]
    fn nothing_is_sent_while_the_terminal_has_focus_or_the_mode_is_off() {
        // Arrange
        let mut focused = notifying(NotifyMode::Bell);
        let mut off = notifying(NotifyMode::Off);
        off.set_focus(false);

        // Act
        focused.handle_event(result_event(false));
        off.handle_event(result_event(false));

        // Assert
        assert_eq!(focused.take_notification(), None);
        assert_eq!(off.take_notification(), None);
    }

    #[test]
    fn a_new_approval_while_out_of_focus_names_the_tool_in_the_notification() {
        // Arrange
        let mut app = notifying(NotifyMode::Osc777);
        app.set_focus(false);

        // Act
        app.handle_event(prompt("r-1"));

        // Assert
        let sent = app.take_notification().unwrap();
        assert!(sent.starts_with("\u{1b}]777;notify;"), "{sent:?}");
        assert!(
            sent.contains("approval") && sent.contains("bash"),
            "{sent:?}"
        );
    }

    #[test]
    fn a_failed_or_stopped_turn_says_so_and_a_replayed_end_says_nothing() {
        // Arrange
        let mut failed = notifying(NotifyMode::Osc9);
        failed.set_focus(false);
        let mut idle = App::new();
        idle.set_notify(NotifyMode::Osc9);
        idle.set_focus(false);

        // Act
        failed.handle_event(result_event(true));
        idle.handle_event(result_event(false));

        // Assert
        assert!(failed.take_notification().unwrap().contains("failed"));
        assert_eq!(idle.take_notification(), None);
    }

    #[test]
    fn a_backend_that_dies_for_good_while_out_of_focus_is_a_notification_and_a_restart_is_not() {
        // Arrange
        let mut dead = notifying(NotifyMode::Osc9);
        dead.set_focus(false);
        let mut restarting = notifying(NotifyMode::Osc9);
        restarting.set_focus(false);

        // Act
        dead.handle_event(crashed(false));
        restarting.handle_event(crashed(true));

        // Assert
        assert!(
            dead.take_notification()
                .unwrap()
                .contains("backend crashed")
        );
        assert_eq!(restarting.take_notification(), None);
    }

    #[test]
    fn losing_the_link_during_a_turn_while_out_of_focus_is_a_notification() {
        // Arrange
        let mut app = notifying(NotifyMode::Osc9);
        app.set_focus(false);

        // Act
        app.handle_event(LinkEvent::Disconnected("gone".to_owned()));

        // Assert
        assert!(app.take_notification().unwrap().contains("connection lost"));
    }

    #[test]
    fn a_request_that_is_already_showing_does_not_ring_again() {
        // Arrange
        let mut app = notifying(NotifyMode::Bell);
        app.set_focus(false);
        app.handle_event(prompt("r-1"));
        app.take_notification();

        // Act
        app.handle_event(prompt("r-1"));

        // Assert
        assert_eq!(app.take_notification(), None);
    }

    #[test]
    fn a_long_tool_name_is_cut_and_the_reason_for_the_notification_is_kept() {
        // Arrange
        let mut app = notifying(NotifyMode::Osc9);
        app.set_focus(false);
        let long = LinkEvent::Permission(PermissionPrompt {
            tool: "t".repeat(300),
            ..match prompt("r-1") {
                LinkEvent::Permission(prompt) => prompt,
                _ => unreachable!(),
            }
        });

        // Act
        app.handle_event(long);

        // Assert
        let sent = app.take_notification().unwrap();
        assert!(sent.contains("needs your approval"), "{sent:?}");
    }

    #[test]
    fn regaining_focus_drops_a_notification_nobody_wrote_yet() {
        // Arrange
        let mut app = notifying(NotifyMode::Bell);
        app.set_focus(false);
        app.handle_event(result_event(false));

        // Act
        app.set_focus(true);

        // Assert
        assert_eq!(app.take_notification(), None);
    }

    #[test]
    fn ctrl_e_expands_the_newest_error() {
        // Arrange
        let (mut app, _commands) = linked();
        app.handle_event(LinkEvent::Notice(
            r#"cannot send: {"error":{"message":"overloaded"}}"#.to_owned(),
        ));

        // Act
        app.handle_key(key(
            KeyCode::Char('e'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        ));
        let shown = wide_screen(&app);

        // Assert
        assert!(
            shown.contains(r#"{"error":{"message":"overloaded"}}"#),
            "{shown}"
        );
    }

    #[test]
    fn a_backend_that_crashes_or_is_interrupted_leaves_its_open_prompt_unanswerable() {
        for gone in [crashed(false), backend_lost()] {
            // Arrange
            let (mut app, mut commands) = armed();
            app.handle_event(prompt("r-1"));

            // Act
            app.handle_event(gone);
            let shown = wide_screen(&app);
            app.handle_key(key(
                KeyCode::Char('y'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            ));

            // Assert
            assert!(shown.contains("backend process is gone"), "{shown}");
            assert_eq!(sent(&mut commands), vec![]);
        }
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

    fn session_in(mode: Mode) -> SessionInfo {
        let mut info = session("s-1");
        info.mode = mode;
        info
    }

    fn plan_delta(turn: &str, text: &str) -> LinkEvent {
        LinkEvent::Session(Event::StreamDelta {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new(turn),
            text: text.to_owned(),
        })
    }

    fn result_of(turn: &str, is_error: bool) -> LinkEvent {
        LinkEvent::Session(Event::Result {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new(turn),
            is_error,
        })
    }

    /// An app in plan mode that holds the plan "step one".
    fn planned() -> (App, UnboundedReceiver<LinkCommand>) {
        let (mut app, commands) = linked();
        app.handle_event(LinkEvent::SessionStarted(session_in(Mode::Plan)));
        app.handle_event(plan_delta("t-1", "step "));
        app.handle_event(plan_delta("t-1", "one"));
        app.handle_event(result_of("t-1", false));
        (app, commands)
    }

    /// An app whose session works in `folder`.
    fn working_in(folder: &std::path::Path) -> (App, UnboundedReceiver<LinkCommand>) {
        let (mut app, commands) = linked();
        let mut info = session("s-1");
        info.cwd = folder.to_string_lossy().into_owned();
        app.handle_event(LinkEvent::SessionStarted(info));
        (app, commands)
    }

    fn key_with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        key(code, modifiers, KeyEventKind::Press)
    }

    #[test]
    fn a_paste_with_lines_fills_the_prompt_across_rows_and_only_enter_sends_it() {
        // Arrange
        let (mut app, mut commands) = linked();

        // Act
        app.handle_paste("first\nsecond\n");
        let shown = wide_screen(&app);
        let before_enter = sent(&mut commands);
        app.handle_key(press(KeyCode::Enter));

        // Assert
        assert!(shown.contains("> first"), "{shown}");
        assert!(shown.contains("  second"), "{shown}");
        assert_eq!(before_enter, Vec::<LinkCommand>::new());
        assert_eq!(
            sent(&mut commands),
            vec![LinkCommand::Send("first\nsecond".to_owned())]
        );
    }

    #[test]
    fn a_paste_with_invisible_characters_drops_them_and_warns() {
        // Arrange
        let (mut app, _commands) = linked();

        // Act
        app.handle_paste("ig\u{200b}nore\u{202e}");
        let shown = wide_screen(&app);

        // Assert
        assert_eq!(app.input.text(), "ignore");
        assert!(shown.contains("removed 2 invisible characters"), "{shown}");
    }

    #[test]
    fn a_large_paste_is_kept_in_a_file_the_prompt_points_to() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let (mut app, _commands) = working_in(dir.path());
        let big = "x".repeat(paste::LARGE_PASTE_BYTES);

        // Act
        app.handle_paste(&big);

        // Assert
        let shown = app.input.text().to_owned();
        assert!(
            shown.starts_with("[pasted text of 32 KiB, saved at .coda/pastes/"),
            "{shown}"
        );
        let relative = shown
            .split("saved at ")
            .nth(1)
            .and_then(|rest| rest.split(':').next())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join(relative)).unwrap(),
            big
        );
    }

    #[test]
    fn a_paste_just_under_the_limit_stays_in_the_prompt() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let (mut app, _commands) = working_in(dir.path());
        let almost = "y".repeat(paste::LARGE_PASTE_BYTES - 1);

        // Act
        app.handle_paste(&almost);

        // Assert
        assert_eq!(app.input.text(), almost);
        assert!(!dir.path().join(".coda").exists());
    }

    #[test]
    fn a_large_paste_that_cannot_be_kept_stays_in_the_prompt_with_a_warning() {
        // Arrange
        let (mut app, _commands) = linked();
        let big = "x".repeat(paste::LARGE_PASTE_BYTES);

        // Act
        app.handle_paste(&big);

        // Assert
        assert_eq!(app.input.text(), big);
        let shown = crate::test_support::screen(120, 24, |frame| app.render(frame));
        assert!(shown.contains("could not keep the large paste"), "{shown}");
    }

    #[test]
    fn the_size_limit_counts_the_paste_as_it_came_and_the_file_keeps_its_tabs() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let (mut app, _commands) = working_in(dir.path());
        let tabs = "\tx".repeat(paste::LARGE_PASTE_BYTES / 2 - 1);

        // Act
        app.handle_paste(&tabs);
        let small = app.input.text().to_owned();
        app.input.clear();
        app.handle_paste(&format!("{tabs}\tyy"));

        // Assert
        assert_eq!(small, tabs.replace('\t', "    "));
        assert!(
            app.input.text().starts_with("[pasted text of "),
            "{}",
            app.input.text()
        );
        let relative = app
            .input
            .text()
            .split("saved at ")
            .nth(1)
            .and_then(|rest| rest.split(':').next())
            .unwrap()
            .to_owned();
        let kept = std::fs::read_to_string(dir.path().join(relative)).unwrap();
        assert!(kept.starts_with("\tx\tx"), "{}", &kept[..20]);
    }

    #[test]
    fn escape_clears_the_prompt_but_keeps_the_draft_in_the_stash() {
        // Arrange
        let (mut app, _commands) = linked();
        app.handle_paste("a long draft");

        // Act
        app.handle_key(press(KeyCode::Esc));
        let cleared = (app.input.text().to_owned(), wide_screen(&app));
        app.handle_key(key_with(KeyCode::Char('s'), KeyModifiers::CONTROL));

        // Assert
        assert_eq!(cleared.0, "");
        assert!(cleared.1.contains("Ctrl+S brings it back"), "{}", cleared.1);
        assert_eq!(app.input.text(), "a long draft");
    }

    #[test]
    fn control_s_with_nothing_to_stash_says_so() {
        // Arrange
        let (mut app, _commands) = linked();

        // Act
        app.handle_key(key_with(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let shown = wide_screen(&app);

        // Assert
        assert!(shown.contains("nothing is stashed"), "{shown}");
    }

    #[test]
    fn control_s_stashes_the_prompt_and_brings_it_back_on_an_empty_line() {
        // Arrange
        let (mut app, _commands) = linked();
        app.handle_paste("half a thought");

        // Act
        app.handle_key(key_with(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let stashed = (app.input.text().to_owned(), wide_screen(&app));
        app.handle_key(key_with(KeyCode::Char('s'), KeyModifiers::CONTROL));

        // Assert
        assert_eq!(stashed.0, "");
        assert!(stashed.1.contains("prompt stashed"), "{}", stashed.1);
        assert_eq!(app.input.text(), "half a thought");
    }

    #[test]
    fn tab_completes_an_at_path_against_the_session_folder() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "x").unwrap();
        let (mut app, _commands) = working_in(dir.path());
        app.handle_paste("see @no");

        // Act
        app.handle_key(press(KeyCode::Tab));

        // Assert
        assert_eq!(app.input.text(), "see @notes.md ");
    }

    #[test]
    fn tab_lists_the_choices_when_several_files_match() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.rs"), "x").unwrap();
        std::fs::write(dir.path().join("alpine.rs"), "x").unwrap();
        let (mut app, _commands) = working_in(dir.path());
        app.handle_paste("@a");

        // Act
        app.handle_key(press(KeyCode::Tab));
        let shown = wide_screen(&app);

        // Assert
        assert_eq!(app.input.text(), "@alp");
        assert!(shown.contains("2 matches: alpha.rs, alpine.rs"), "{shown}");
    }

    #[test]
    fn tab_says_when_nothing_matches() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let (mut app, _commands) = working_in(dir.path());
        app.handle_paste("@zzz");

        // Act
        app.handle_key(press(KeyCode::Tab));
        let shown = wide_screen(&app);

        // Assert
        assert!(shown.contains("no file matches @zzz"), "{shown}");
        assert_eq!(app.input.text(), "@zzz");
    }

    #[test]
    fn tab_leaves_a_word_that_is_not_a_path_alone() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let (mut app, _commands) = working_in(dir.path());
        app.handle_paste("plain");

        // Act
        app.handle_key(press(KeyCode::Tab));

        // Assert
        assert_eq!(app.input.text(), "plain");
    }

    #[test]
    fn tab_inside_a_word_changes_nothing() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "x").unwrap();
        let (mut app, _commands) = working_in(dir.path());
        app.handle_paste("@notes.md");
        app.handle_key(press(KeyCode::Left));

        // Act
        app.handle_key(press(KeyCode::Tab));

        // Assert
        assert_eq!(app.input.text(), "@notes.md");
    }

    #[test]
    fn tab_before_a_session_has_started_says_there_is_no_folder() {
        // Arrange
        let (mut app, _commands) = linked();
        app.handle_paste("@no");

        // Act
        app.handle_key(press(KeyCode::Tab));
        let shown = wide_screen(&app);

        // Assert
        assert!(shown.contains("no session folder yet"), "{shown}");
        assert_eq!(app.input.text(), "@no");
    }

    #[test]
    fn tab_goes_to_a_prompt_that_is_open_and_does_not_complete() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.md"), "x").unwrap();
        let (mut app, _commands) = working_in(dir.path());
        app.handle_paste("@no");
        app.handle_event(prompt("r-1"));

        // Act
        app.handle_key(press(KeyCode::Tab));

        // Assert
        assert_eq!(app.input.text(), "@no");
    }

    #[test]
    fn the_prompt_grows_with_its_lines_and_leaves_the_transcript_the_rest() {
        // Arrange
        let (mut app, _commands) = linked();
        app.handle_paste("one\ntwo\nthree");

        // Act
        let shown = crate::test_support::screen(40, 12, |frame| app.render(frame));

        // Assert
        let rows: Vec<&str> = shown.lines().collect();
        assert!(rows[rows.len() - 5].starts_with("> one"), "{shown}");
        assert!(rows[rows.len() - 4].starts_with("  two"), "{shown}");
        assert!(rows[rows.len() - 3].starts_with("  three"), "{shown}");
    }

    #[test]
    fn plan_asks_the_link_to_change_the_mode() {
        // Arrange
        let (mut app, mut commands) = linked();

        // Act
        type_line(&mut app, "/plan");

        // Assert
        assert_eq!(
            sent(&mut commands),
            vec![LinkCommand::SetMode {
                mode: Mode::Plan,
                then: None
            }]
        );
        assert!(!app.turn_running);
    }

    #[test]
    fn plan_in_plan_mode_changes_nothing_and_says_so() {
        // Arrange
        let (mut app, mut commands) = linked();
        app.handle_event(LinkEvent::SessionStarted(session_in(Mode::Plan)));

        // Act
        type_line(&mut app, "/plan");

        // Assert
        assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
        let shown = wide_screen(&app);
        assert!(shown.contains("already in plan mode"), "{shown}");
    }

    #[test]
    fn implement_sends_the_plan_back_as_a_default_mode_turn() {
        // Arrange
        let (mut app, mut commands) = planned();

        // Act
        type_line(&mut app, "/implement");

        // Assert
        assert_eq!(
            sent(&mut commands),
            vec![LinkCommand::SetMode {
                mode: Mode::Default,
                then: Some(commands::implement_prompt(Some("step one")))
            }]
        );
        assert!(!app.turn_running);
    }

    #[test]
    fn implement_outside_plan_mode_changes_nothing_and_says_so() {
        for mode in [Mode::Default, Mode::AcceptEdits] {
            // Arrange
            let (mut app, mut commands) = linked();
            app.handle_event(LinkEvent::SessionStarted(session_in(mode)));

            // Act
            type_line(&mut app, "/implement");

            // Assert
            assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
            let shown = wide_screen(&app);
            assert!(shown.contains("not in plan mode"), "{shown}");
        }
    }

    #[test]
    fn a_finished_plan_turn_makes_the_plan_ready() {
        // Arrange
        let (app, _commands) = planned();

        // Act
        let shown = wide_screen(&app);

        // Assert
        assert!(shown.contains("plan ready"), "{shown}");
    }

    #[test]
    fn a_turn_that_failed_or_ran_outside_plan_mode_leaves_no_plan() {
        // Arrange
        let mut failed = App::new();
        failed.handle_event(LinkEvent::SessionStarted(session_in(Mode::Plan)));
        failed.handle_event(plan_delta("t-1", "half a plan"));
        failed.handle_event(result_of("t-1", true));
        let mut outside = App::new();
        outside.handle_event(LinkEvent::SessionStarted(session("s-1")));
        outside.handle_event(plan_delta("t-1", "an answer"));
        outside.handle_event(result_of("t-1", false));

        // Act
        let (failed_shown, outside_shown) = (wide_screen(&failed), wide_screen(&outside));

        // Assert
        assert!(!failed_shown.contains("plan ready"), "{failed_shown}");
        assert!(!outside_shown.contains("plan ready"), "{outside_shown}");
    }

    #[test]
    fn a_newer_plan_turn_replaces_the_plan_and_a_new_session_clears_it() {
        // Arrange
        let (mut app, mut commands) = planned();
        app.handle_event(plan_delta("t-2", "step two"));
        app.handle_event(result_of("t-2", false));

        // Act
        type_line(&mut app, "/implement");
        let replaced = sent(&mut commands);
        app.handle_event(LinkEvent::SessionStarted(session_in(Mode::Plan)));
        type_line(&mut app, "/plan save out.md");

        // Assert
        assert_eq!(
            replaced,
            vec![LinkCommand::SetMode {
                mode: Mode::Default,
                then: Some(commands::implement_prompt(Some("step two")))
            }]
        );
        let shown = wide_screen(&app);
        assert!(shown.contains("no plan to save"), "{shown}");
    }

    #[test]
    fn plan_save_writes_the_plan_and_refuses_to_overwrite() {
        // Arrange
        let (mut app, _commands) = planned();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plan.md");
        let line = format!("/plan save {}", path.display());

        // Act
        type_line(&mut app, &line);
        let written = std::fs::read_to_string(&path).unwrap();
        type_line(&mut app, &line);

        // Assert
        assert_eq!(written, "step one\n");
        let shown = crate::test_support::screen(300, 8, |frame| app.render(frame));
        assert!(shown.contains("plan written to"), "{shown}");
        assert!(shown.contains("could not save the plan"), "{shown}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "step one\n");
    }

    #[test]
    fn a_mode_change_is_refused_while_a_turn_is_running() {
        // Arrange
        let (mut app, mut commands) = linked();
        type_line(&mut app, "q hi");
        sent(&mut commands);

        // Act
        type_line(&mut app, "/plan");
        type_line(&mut app, "/implement");

        // Assert
        assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
        let shown = wide_screen(&app);
        assert!(shown.contains("wait for the running turn"), "{shown}");
    }

    #[test]
    fn a_daemon_that_cannot_change_the_mode_is_not_asked() {
        // Arrange
        let (mut app, mut commands) = linked();
        app.handle_event(LinkEvent::Capabilities(Capabilities::default()));

        // Act
        type_line(&mut app, "/plan");

        // Assert
        assert_eq!(sent(&mut commands), Vec::<LinkCommand>::new());
        let shown = wide_screen(&app);
        assert!(shown.contains("cannot change the mode"), "{shown}");
    }

    #[test]
    fn a_changed_mode_shows_in_the_status_line_and_the_transcript() {
        // Arrange
        let mut app = App::new();
        app.handle_event(LinkEvent::SessionStarted(session("s-1")));

        // Act
        app.handle_event(LinkEvent::ModeChanged {
            info: session_in(Mode::Plan),
            started_turn: false,
        });

        // Assert
        let shown = wide_screen(&app);
        assert!(shown.contains("plan mode"), "{shown}");
        assert!(shown.contains("mode is now plan"), "{shown}");
        assert!(!app.turn_running);
    }

    #[test]
    fn a_resumed_session_in_plan_mode_shows_plan_mode() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_event(LinkEvent::SessionStarted(session_in(Mode::Plan)));

        // Assert
        let shown = wide_screen(&app);
        assert!(shown.contains("plan mode"), "{shown}");
    }

    #[test]
    fn a_mode_change_that_started_a_turn_marks_the_turn_running() {
        // Arrange
        let mut app = App::new();

        // Act
        app.handle_event(LinkEvent::ModeChanged {
            info: session_in(Mode::Default),
            started_turn: true,
        });

        // Assert
        assert!(app.turn_running);
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
    fn the_review_command_sends_a_prompt_that_hands_the_change_to_the_reviewer() {
        // Arrange
        let (mut app, mut commands) = linked();

        // Act
        type_line(&mut app, "/review the parser change");

        // Assert
        let all = sent(&mut commands);
        let [LinkCommand::Send(prompt)] = all.as_slice() else {
            panic!("expected one message");
        };
        assert!(prompt.contains("the parser change"), "{prompt}");
        assert!(prompt.contains("\"reviewer\""), "{prompt}");
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

    fn tool_event(id: &str, name: &str, summary: &str, status: ToolCallStatus) -> LinkEvent {
        LinkEvent::Session(Event::ToolCall {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            id: id.to_owned(),
            name: name.to_owned(),
            summary: Some(summary.to_owned()),
            status,
        })
    }

    fn turn_result() -> LinkEvent {
        LinkEvent::Session(Event::Result {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("t-1"),
            is_error: false,
        })
    }

    #[test]
    fn the_running_tool_shows_on_a_live_line_with_its_time() {
        // Arrange
        let mut app = App::new();
        let t0 = Instant::now();

        // Act
        app.handle_event_at(
            tool_event("1", "Bash", "cargo test", ToolCallStatus::Started),
            t0,
        );
        let shown = screen_at(&app, t0 + Duration::from_secs(12));

        // Assert
        assert!(shown.contains("running cargo test 12s"), "{shown}");
    }

    #[test]
    fn the_live_line_goes_when_the_tool_ends() {
        // Arrange
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle_event_at(tool_event("1", "Bash", "ls", ToolCallStatus::Started), t0);

        // Act
        app.handle_event_at(tool_event("1", "Bash", "ls", ToolCallStatus::Succeeded), t0);

        // Assert
        let shown = screen_at(&app, t0 + Duration::from_secs(5));
        assert!(!shown.contains("running"), "{shown}");
    }

    #[test]
    fn a_settled_turn_folds_into_one_line_in_the_transcript() {
        // Arrange
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle_event_at(tool_event("1", "Bash", "ls", ToolCallStatus::Started), t0);
        app.handle_event_at(tool_event("1", "Bash", "ls", ToolCallStatus::Succeeded), t0);
        app.handle_event_at(tool_event("2", "Edit", "a.rs", ToolCallStatus::Started), t0);
        app.handle_event_at(
            tool_event("2", "Edit", "a.rs", ToolCallStatus::Succeeded),
            t0,
        );

        // Act
        app.handle_event_at(turn_result(), t0 + Duration::from_secs(192));

        // Assert
        let shown = screen_at(&app, t0 + Duration::from_secs(192));
        assert!(
            shown.contains("Worked for 3m 12s: ran 1 command"),
            "{shown}"
        );
        assert!(shown.contains("ctrl+o to expand"), "{shown}");
    }

    #[test]
    fn ctrl_o_expands_the_fold_and_again_folds_it() {
        // Arrange
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle_event_at(
            tool_event("1", "Bash", "ls -la", ToolCallStatus::Started),
            t0,
        );
        app.handle_event_at(
            tool_event("1", "Bash", "ls -la", ToolCallStatus::Succeeded),
            t0,
        );
        app.handle_event_at(turn_result(), t0);
        let ctrl_o = key(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        );

        // Act
        app.handle_key(ctrl_o);
        let open = screen(&app);
        app.handle_key(ctrl_o);
        let closed = screen(&app);

        // Assert
        assert!(open.contains("Bash: ls -la"), "{open}");
        assert!(!closed.contains("Bash: ls -la"), "{closed}");
        assert_eq!(app.input.text(), "");
    }

    #[test]
    fn a_pending_approval_shows_a_wait_and_no_fold() {
        // Arrange
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle_event_at(tool_event("1", "Bash", "rm x", ToolCallStatus::Started), t0);

        // Act
        app.handle_event_at(prompt("r-1"), t0);
        let waiting = app.work_line(t0 + Duration::from_secs(30));

        // Assert
        assert_eq!(waiting.as_deref(), Some("waiting for approval"));
    }

    #[test]
    fn an_interrupted_turn_says_stopped_and_never_finished() {
        // Arrange
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle_event_at(
            tool_event("1", "Bash", "sleep 9", ToolCallStatus::Started),
            t0,
        );

        // Act
        app.handle_event_at(turn_ended(), t0 + Duration::from_secs(3));

        // Assert
        let shown = screen_at(&app, t0 + Duration::from_secs(3));
        assert!(shown.contains("Stopped after 3s: 1 cut short"), "{shown}");
        assert!(!shown.contains("finished"), "{shown}");
    }

    #[test]
    fn a_message_sent_during_a_turn_keeps_the_work_so_far() {
        // Arrange
        let (mut app, _commands) = linked();
        type_line(&mut app, "go");
        let t0 = Instant::now();
        app.handle_event_at(tool_event("1", "Bash", "ls", ToolCallStatus::Started), t0);

        // Act
        type_line(&mut app, "again");

        // Assert
        assert_eq!(
            app.work_line(t0 + Duration::from_secs(2)).as_deref(),
            Some("running ls 2s")
        );
    }

    fn todo(id: u32, text: &str, status: coda_protocol::events::TodoStatus) -> TodoItem {
        TodoItem {
            id,
            text: text.to_owned(),
            status,
        }
    }

    #[test]
    fn the_todo_list_is_drawn_above_the_prompt_and_a_new_session_clears_it() {
        // Arrange
        use coda_protocol::events::TodoStatus;
        let mut app = App::new();
        app.handle_event(LinkEvent::Todos(vec![
            todo(1, "plan", TodoStatus::Done),
            todo(2, "build", TodoStatus::InProgress),
        ]));

        // Act
        let shown = screen(&app);
        app.handle_event(LinkEvent::SessionStarted(session("s-2")));
        let cleared = screen(&app);

        // Assert
        assert!(shown.contains("[x] plan"), "{shown}");
        assert!(shown.contains("[~] build"), "{shown}");
        assert!(!cleared.contains("plan"), "{cleared}");
    }

    #[test]
    fn a_new_session_drops_the_work_of_the_old_one() {
        // Arrange
        let mut app = App::new();
        let t0 = Instant::now();
        app.handle_event_at(tool_event("1", "Bash", "ls", ToolCallStatus::Started), t0);

        // Act
        app.handle_event_at(LinkEvent::SessionStarted(session("s-2")), t0);

        // Assert
        assert_eq!(app.work_line(t0), None);
    }

    fn wide_screen_at(app: &App, now: Instant) -> String {
        crate::test_support::screen(120, 8, |frame| app.render_at(frame, now))
    }

    /// An app with a turn that began at `t0`.
    fn working(t0: Instant) -> (App, UnboundedReceiver<LinkCommand>) {
        let (mut app, mut commands) = linked();
        type_line(&mut app, "go");
        sent(&mut commands);
        app.work.begin(t0);
        (app, commands)
    }

    #[test]
    fn a_running_turn_shows_working_with_its_time_on_the_status_line() {
        // Arrange
        let t0 = Instant::now();
        let (app, _commands) = working(t0);

        // Act
        let shown = wide_screen_at(&app, t0 + Duration::from_secs(64));

        // Assert
        assert!(shown.contains("| Working 1m 04s"), "{shown}");
    }

    #[test]
    fn an_idle_app_shows_no_working_state() {
        let app = App::new();

        let shown = wide_screen_at(&app, Instant::now());

        assert!(!shown.contains("Working"), "{shown}");
    }

    #[test]
    fn ctrl_c_in_a_turn_shows_stopping_then_offers_the_force_after_the_timeout() {
        // Arrange
        let t0 = Instant::now();
        let (mut app, _commands) = working(t0);

        // Act
        app.handle_key_at(ctrl_c(), t0);
        let early = wide_screen_at(&app, t0 + Duration::from_secs(1));
        let late = wide_screen_at(&app, t0 + activity::STOP_TIMEOUT);

        // Assert
        assert!(early.contains("Stopping"), "{early}");
        assert!(!early.contains("Working"), "{early}");
        assert!(
            late.contains("not responding, press Ctrl+C again to force stop"),
            "{late}"
        );
    }

    #[test]
    fn a_second_ctrl_c_inside_the_timeout_cancels_again_and_after_it_forces() {
        // Arrange
        let t0 = Instant::now();
        let (mut app, mut commands) = working(t0);

        // Act
        app.handle_key_at(ctrl_c(), t0);
        app.handle_key_at(ctrl_c(), t0 + Duration::from_secs(1));
        let early = sent(&mut commands);
        app.handle_key_at(ctrl_c(), t0 + activity::STOP_TIMEOUT);
        let forced = sent(&mut commands);
        let after = wide_screen_at(&app, t0 + activity::STOP_TIMEOUT);

        // Assert
        assert_eq!(early, vec![LinkCommand::Cancel, LinkCommand::Cancel]);
        assert_eq!(forced, vec![LinkCommand::ForceCancel]);
        assert!(after.contains("Forcing stop"), "{after}");
    }

    #[test]
    fn a_press_right_after_the_force_cancels_again_without_forcing() {
        // Arrange
        let t0 = Instant::now();
        let (mut app, mut commands) = working(t0);
        app.handle_key_at(ctrl_c(), t0);
        app.handle_key_at(ctrl_c(), t0 + activity::STOP_TIMEOUT);
        sent(&mut commands);

        // Act
        app.handle_key_at(
            ctrl_c(),
            t0 + activity::STOP_TIMEOUT + Duration::from_secs(1),
        );

        // Assert
        assert_eq!(sent(&mut commands), vec![LinkCommand::Cancel]);
    }

    #[test]
    fn a_force_that_did_not_end_the_turn_is_offered_and_sent_again() {
        // Arrange
        let t0 = Instant::now();
        let (mut app, mut commands) = working(t0);
        app.handle_key_at(ctrl_c(), t0);
        let forced_at = t0 + activity::STOP_TIMEOUT;
        app.handle_key_at(ctrl_c(), forced_at);
        sent(&mut commands);
        let again_at = forced_at + activity::STOP_TIMEOUT;

        // Act
        let offered = wide_screen_at(&app, again_at);
        app.handle_key_at(ctrl_c(), again_at);

        // Assert
        assert!(
            offered.contains("press Ctrl+C again to force stop"),
            "{offered}"
        );
        assert_eq!(sent(&mut commands), vec![LinkCommand::ForceCancel]);
    }

    #[test]
    fn a_daemon_that_cannot_force_never_gets_a_force_or_an_offer() {
        // Arrange
        let t0 = Instant::now();
        let (mut app, mut commands) = working(t0);
        app.handle_event_at(LinkEvent::Capabilities(Capabilities::default()), t0);
        app.handle_key_at(ctrl_c(), t0);

        // Act
        let late = t0 + activity::STOP_TIMEOUT;
        let shown = wide_screen_at(&app, late);
        app.handle_key_at(ctrl_c(), late);

        // Assert
        assert!(shown.contains("not responding"), "{shown}");
        assert!(!shown.contains("force stop"), "{shown}");
        assert_eq!(
            sent(&mut commands),
            vec![LinkCommand::Cancel, LinkCommand::Cancel]
        );
    }

    #[test]
    fn the_stop_state_goes_when_the_turn_ends_and_does_not_reach_the_next_turn() {
        // Arrange
        let t0 = Instant::now();
        let (mut app, mut commands) = working(t0);
        app.handle_key_at(ctrl_c(), t0);

        // Act
        app.handle_event_at(turn_ended(), t0 + Duration::from_secs(1));
        let idle = wide_screen_at(&app, t0 + Duration::from_secs(2));
        type_line(&mut app, "again");
        sent(&mut commands);
        let next = wide_screen_at(&app, Instant::now());

        // Assert
        assert!(!idle.contains("Stopping"), "{idle}");
        assert!(next.contains("Working"), "{next}");
        assert!(!next.contains("Stopping"), "{next}");
    }
}
