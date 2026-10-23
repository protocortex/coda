// SPDX-License-Identifier: AGPL-3.0-only

//! Terminal UI for coda.

pub mod app;
pub mod chat;
pub mod commands;
pub mod input;
pub mod logs;
pub mod permission;
pub mod sessions;
pub mod status;
pub mod terminal;

#[cfg(test)]
mod test_support;

use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use coda_config::timing::Timing;
use crossterm::event::{self, Event, KeyEvent};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tracing::instrument::WithSubscriber;
use tracing::{Dispatch, warn};

use app::{App, LinkEvent};
use commands::UserCommands;
use logs::LogBuffer;
use sessions::{LinkCommand, LinkConfig, run_link};
use terminal::{AnsiMode, SystemRawMode, TerminalGuard, install_panic_hook, restore_stdout};

const KEY_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// How long quitting waits for the link to close the session it created.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Why the UI stopped with an error.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("terminal I/O failed")]
    Terminal(#[from] io::Error),
    #[error("drawing the screen failed")]
    Draw(#[source] BoxError),
    #[error("the UI task ended abnormally")]
    Task(#[from] tokio::task::JoinError),
}

/// What the user did at the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserInput {
    Key(KeyEvent),
    /// Text pasted in one piece, as the terminal delivers it in bracketed paste mode.
    Paste(String),
}

/// A source of user input that gives up after a short wait so the UI can redraw.
pub trait KeySource {
    /// Returns `None` when nothing arrived within the wait.
    ///
    /// # Errors
    /// Returns the I/O error of the terminal.
    fn next_input(&mut self) -> io::Result<Option<UserInput>>;
}

/// Reads key presses from the real terminal.
pub struct TerminalKeys;

impl KeySource for TerminalKeys {
    fn next_input(&mut self) -> io::Result<Option<UserInput>> {
        if !event::poll(KEY_POLL_INTERVAL)? {
            return Ok(None);
        }
        match event::read()? {
            Event::Key(key) => Ok(Some(UserInput::Key(key))),
            Event::Paste(text) => Ok(Some(UserInput::Paste(text))),
            _ => Ok(None),
        }
    }
}

/// Applies pending link events, draws `app`, then feeds it input from `keys`
/// until it asks to quit.
///
/// # Errors
/// Returns [`TuiError::Draw`] if drawing fails or [`TuiError::Terminal`] if reading keys does.
pub fn drive<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    keys: &mut impl KeySource,
    events: &mut mpsc::UnboundedReceiver<LinkEvent>,
) -> Result<(), TuiError>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    while !app.should_quit() {
        while let Ok(event) = events.try_recv() {
            app.handle_event(event);
        }
        draw(terminal, app)?;
        match keys.next_input()? {
            Some(UserInput::Key(key)) => app.handle_key(key),
            Some(UserInput::Paste(text)) => app.handle_paste(&text),
            None => {}
        }
    }
    Ok(())
}

fn draw<B: Backend>(terminal: &mut Terminal<B>, app: &App) -> Result<(), TuiError>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    terminal
        .draw(|frame| app.render(frame))
        .map(|_| ())
        .map_err(|error| TuiError::Draw(Box::new(error)))
}

/// What the UI is told about the user's config when it starts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Startup {
    /// No provider is enabled, so the transcript starts with the setup guidance.
    pub needs_setup: bool,
    /// The config has hooks, so a session on a backend that runs none gets a notice.
    pub hooks_configured: bool,
}

/// Shows the connection state and runs until the user quits or `shutdown`
/// completes. `startup` says what the config needs the UI to show, and
/// `user_commands` are the slash commands from command files. The
/// connection is made in the background, so the keys work while it is pending;
/// a failed or lost connection is shown, not returned.
///
/// # Errors
/// Returns [`TuiError`] if the terminal fails or the UI task panics.
pub async fn run_session<B, K>(
    mut terminal: Terminal<B>,
    socket_path: &Path,
    timing: Timing,
    startup: Startup,
    user_commands: UserCommands,
    mut keys: K,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<App, TuiError>
where
    B::Error: std::error::Error + Send + Sync + 'static,
    B: Backend + Send + 'static,
    K: KeySource + Send + 'static,
{
    let (commands, command_rx) = mpsc::unbounded_channel();
    let mut app = App::with_link(commands.clone());
    app.set_user_commands(user_commands);
    if startup.needs_setup {
        app.show_setup_guidance();
    }
    draw(&mut terminal, &app)?;
    let (sender, mut events) = mpsc::unbounded_channel();
    let dispatch = tracing::dispatcher::get_default(Dispatch::clone);
    let mut link = tokio::spawn(
        run_link(
            socket_path.to_path_buf(),
            timing,
            LinkConfig {
                hooks_configured: startup.hooks_configured,
                ..LinkConfig::default()
            },
            command_rx,
            sender.clone(),
        )
        .with_subscriber(dispatch),
    );
    let stop = tokio::spawn(async move {
        shutdown.await;
        let _ = sender.send(LinkEvent::Quit);
    });
    let driven = tokio::task::spawn_blocking(move || {
        drive(&mut terminal, &mut app, &mut keys, &mut events).map(|()| app)
    })
    .await;
    // The link closes the session it created before it ends; a daemon that does
    // not answer in time is left to evict it, and aborting drops the client.
    let _ = commands.send(LinkCommand::Close);
    let _ = tokio::time::timeout(CLOSE_GRACE, &mut link).await;
    link.abort();
    stop.abort();
    driven?
}

/// Completes when the process is asked to terminate or its terminal hangs up.
fn termination() -> io::Result<impl Future<Output = ()> + Send + 'static> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    Ok(async move {
        tokio::select! {
            _ = terminate.recv() => {}
            _ = hangup.recv() => {}
        }
    })
}

/// A session error outranks a restore error, which is only logged, because the
/// session error is the one that explains why the UI stopped.
fn finish(session: Result<(), TuiError>, restored: io::Result<()>) -> Result<(), TuiError> {
    match (session, restored) {
        (Ok(()), restored) => restored.map_err(TuiError::from),
        (Err(error), restored) => {
            if let Err(restore_error) = restored {
                warn!(error = %restore_error, "could not restore the terminal");
            }
            Err(error)
        }
    }
}

/// Runs the UI on the real terminal, restoring it however the UI ends: a normal
/// exit, an error, a panic or SIGTERM/SIGHUP.
///
/// Log output is held back while the terminal is in raw mode and printed to
/// stderr after it is restored. `startup` and `user_commands` are passed on to
/// [`run_session`].
///
/// # Errors
/// Returns [`TuiError`] if the terminal cannot be set up or fails while running.
pub async fn run(
    socket_path: &Path,
    timing: Timing,
    startup: Startup,
    user_commands: UserCommands,
) -> Result<(), TuiError> {
    let shutdown = termination()?;
    let logs = LogBuffer::default();
    let guard = TerminalGuard::new(AnsiMode::new(io::stdout(), SystemRawMode))?;
    install_panic_hook(restore_stdout);
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let session = run_session(
        terminal,
        socket_path,
        timing,
        startup,
        user_commands,
        TerminalKeys,
        shutdown,
    )
    .with_subscriber(logs.dispatch())
    .await;
    let restored = guard.restore();
    // Best effort: the terminal is already back, and there is no better place to report.
    let _ = io::stderr().write_all(&logs.take());
    finish(session.map(|_| ()), restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_error() -> TuiError {
        TuiError::Terminal(io::Error::other("session failed"))
    }

    #[test]
    fn a_session_error_wins_over_a_restore_error() {
        // Arrange
        let restored = Err(io::Error::other("restore failed"));

        // Act
        let result = finish(Err(session_error()), restored);

        // Assert
        let message = format!("{:?}", result.unwrap_err());
        assert!(message.contains("session failed"), "{message}");
    }

    #[test]
    fn a_restore_error_is_returned_when_the_session_succeeded() {
        // Arrange
        let restored = Err(io::Error::other("restore failed"));

        // Act
        let result = finish(Ok(()), restored);

        // Assert
        let message = format!("{:?}", result.unwrap_err());
        assert!(message.contains("restore failed"), "{message}");
    }

    #[test]
    fn a_clean_session_and_restore_succeed() {
        // Arrange / Act
        let result = finish(Ok(()), Ok(()));

        // Assert
        assert!(result.is_ok());
    }
}
