// SPDX-License-Identifier: AGPL-3.0-only

//! The UI session against a real daemon server on a temporary socket.

use std::collections::VecDeque;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, Instant};

use coda_config::timing::Timing;
use coda_daemon::server::{Server, ServerConfig};
use coda_tui::app::ConnectionState;
use coda_tui::logs::LogBuffer;
use coda_tui::{KeySource, UserInput, drive, run_session};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};
use tracing::instrument::WithSubscriber;

const WAIT: Duration = Duration::from_secs(5);

/// Plays back scripted polls: `None` is a quiet tick, `Some` a key press.
struct Script(VecDeque<Option<KeyEvent>>);

impl Script {
    fn quit_after_ticks(ticks: usize) -> Self {
        let mut polls: VecDeque<_> = std::iter::repeat_n(None, ticks).collect();
        polls.push_back(Some(KeyEvent {
            code: KeyCode::Esc,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }));
        Self(polls)
    }
}

impl KeySource for Script {
    fn next_input(&mut self) -> io::Result<Option<UserInput>> {
        Ok(self.0.pop_front().flatten().map(UserInput::Key))
    }
}

/// Quiet polls that last like the real ones, then a quit key once `delay` has passed.
struct QuitAfter(Instant);

impl QuitAfter {
    fn new(delay: Duration) -> Self {
        Self(Instant::now() + delay)
    }
}

impl KeySource for QuitAfter {
    fn next_input(&mut self) -> io::Result<Option<UserInput>> {
        std::thread::sleep(Duration::from_millis(10));
        Ok(
            (Instant::now() >= self.0).then_some(UserInput::Key(KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })),
        )
    }
}

fn terminal() -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(40, 5)).unwrap()
}

/// macOS limits socket paths to 104 bytes, so keep them under /tmp.
fn short_dir() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("ct")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("timed out waiting")
}

#[test]
fn drive_returns_once_the_quit_key_arrives() {
    // Arrange
    let mut terminal = terminal();
    let mut app = coda_tui::app::App::new();
    let mut keys = Script::quit_after_ticks(2);
    let (_sender, mut events) = mpsc::unbounded_channel();

    // Act
    drive(&mut terminal, &mut app, &mut keys, &mut events).unwrap();

    // Assert
    assert!(app.should_quit());
    assert!(keys.0.is_empty());
}

fn never() -> impl Future<Output = ()> + Send + 'static {
    std::future::pending()
}

#[tokio::test(flavor = "multi_thread")]
async fn it_connects_to_a_running_daemon_and_exits_on_quit() {
    // Arrange
    let dir = short_dir();
    let uid = std::fs::metadata(dir.path()).unwrap().uid();
    let config = ServerConfig::new(dir.path().join("s.sock"), uid);
    let socket = config.socket_path.clone();
    let bound = Server::bind(config).unwrap();
    let (shutdown, rx) = oneshot::channel();
    let server = tokio::spawn(bound.serve(async {
        let _ = rx.await;
    }));

    // Act
    let app = within(run_session(
        terminal(),
        &socket,
        Timing::for_tests(),
        QuitAfter::new(Duration::from_millis(500)),
        never(),
    ))
    .await
    .unwrap();

    // Assert
    assert_eq!(app.connection(), &ConnectionState::Connected);
    assert!(app.should_quit());
    let _ = shutdown.send(());
    within(server).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn it_shows_a_failed_connection_and_still_exits_on_quit() {
    // Arrange
    let dir = short_dir();
    let missing = dir.path().join("none.sock");
    let logs = LogBuffer::default();

    // Act
    let app = within(
        run_session(
            terminal(),
            Path::new(&missing),
            Timing::for_tests(),
            QuitAfter::new(Duration::from_millis(300)),
            never(),
        )
        .with_subscriber(logs.dispatch()),
    )
    .await
    .unwrap();

    // Assert
    assert!(matches!(app.connection(), ConnectionState::Failed(_)));
    assert!(app.should_quit());
    let captured = String::from_utf8(logs.take()).unwrap();
    assert!(captured.contains("could not connect"), "{captured}");
}

#[tokio::test(flavor = "multi_thread")]
async fn quitting_works_while_a_daemon_that_never_answers_is_still_connecting() {
    // Arrange
    let dir = short_dir();
    let socket = dir.path().join("hung.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let accepting = tokio::spawn(async move {
        // Keep every accepted stream open and silent.
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let mut timing = Timing::for_tests();
    timing.idle_timeout = Duration::from_secs(60);

    // Act
    let app = within(run_session(
        terminal(),
        &socket,
        timing,
        QuitAfter::new(Duration::from_millis(100)),
        never(),
    ))
    .await
    .unwrap();

    // Assert
    assert_eq!(app.connection(), &ConnectionState::Connecting);
    assert!(app.should_quit());
    accepting.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn it_shows_a_disconnect_when_the_daemon_dies_after_connecting() {
    // Arrange
    let dir = short_dir();
    let uid = std::fs::metadata(dir.path()).unwrap().uid();
    let config = ServerConfig::new(dir.path().join("s.sock"), uid);
    let socket = config.socket_path.clone();
    let bound = Server::bind(config).unwrap();
    let server = tokio::spawn(bound.serve(std::future::pending()));
    let session_socket = socket.clone();
    let session = tokio::spawn(async move {
        run_session(
            terminal(),
            &session_socket,
            Timing::for_tests(),
            QuitAfter::new(Duration::from_secs(3)),
            never(),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Act
    server.abort();
    let app = within(session).await.unwrap().unwrap();

    // Assert
    assert!(
        matches!(app.connection(), ConnectionState::Disconnected(_)),
        "{:?}",
        app.connection()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shutdown_signal_ends_the_session_without_a_key() {
    // Arrange
    let dir = short_dir();
    let missing = dir.path().join("none.sock");
    let signal = tokio::time::sleep(Duration::from_millis(100));

    // Act
    let app = within(run_session(
        terminal(),
        &missing,
        Timing::for_tests(),
        QuitAfter::new(Duration::from_secs(60)),
        signal,
    ))
    .await
    .unwrap();

    // Assert
    assert!(app.should_quit());
}
