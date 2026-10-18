// SPDX-License-Identifier: AGPL-3.0-only

//! What a restarted `coda daemon` recovers after the previous one died with
//! SIGKILL in the middle of a turn that was waiting for a permission answer.

mod common;

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use coda_config::config::Dirs;
use coda_protocol::methods::{
    DAEMON_STATUS, DaemonStatusParams, DaemonStatusResult, SESSION_CREATE, SESSION_RESUME,
    SESSION_SEND, SessionCreateParams, SessionCreateResult, SessionResumeParams,
    SessionResumeResult, SessionSendParams, SessionSendResult,
};
use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::Store;
use common::{Daemon, POLL, WAIT, connect, next_matching, short_dir, start_daemon};
use serde_json::Value;

const PERMISSION_PROMPT: &str = "hello";
const MID_TURN_PROMPT: &str = "permission.request";
const INTERRUPTED_KIND: &str = "turn_interrupted";
const TIMEOUT_KIND: &str = "timeout";
const EVENT_METHOD: &str = "session.event";
const REPLAY_LIMIT: u32 = 100;
const ALREADY_RUNNING_MESSAGE: &str = "already holds the lock";
/// Long enough for a live daemon to answer, short enough to fail fast.
const SERVING_PROBE: Duration = Duration::from_secs(1);

/// Polls `probe` until it returns a value, failing the test after [`WAIT`].
async fn eventually<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(found) = probe() {
                return found;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what} did not happen in time"))
}

/// A second daemon on the same home, with its error output captured.
fn spawn_rival(home: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_coda"))
        .arg("daemon")
        .env("CODA_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("coda binary should start");
    Daemon {
        child,
        run_dir: Dirs::from_base(home).run_dir(),
    }
}

fn sigkill(daemon: &mut Daemon) {
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
}

/// Starts a daemon, leaves a turn waiting for a permission answer and kills it.
/// The request is announced only once it is stored, so it survives the kill.
async fn kill_daemon_mid_turn(home: &Path, work: &Path) -> SessionId {
    let mut daemon = start_daemon(home);
    let client = connect(home).await;
    let created: SessionCreateResult = client
        .request(
            SESSION_CREATE,
            &SessionCreateParams {
                backend: BackendKind::ClaudeCode,
                cwd: work.display().to_string(),
                model: None,
                mode: Mode::Default,
            },
        )
        .await
        .unwrap();
    let session = created.session.id;
    let mut events = client.subscribe(&session, Some(0)).await.unwrap();
    let _: Value = client
        .request(
            SESSION_SEND,
            &SessionSendParams {
                session_id: session.clone(),
                text: PERMISSION_PROMPT.to_owned(),
                max_turns: None,
                token_cap: None,
            },
        )
        .await
        .unwrap();
    next_matching(&mut events, |method, _| method == MID_TURN_PROMPT).await;
    sigkill(&mut daemon);
    session
}

/// How many stored events of each wanted kind the session has.
fn stored_counts(home: &Path, session: &SessionId) -> (usize, usize) {
    let store = Store::open(&Dirs::from_base(home).data_dir()).unwrap();
    let events = store.load_events(session, 0, REPLAY_LIMIT).unwrap();
    let count = |kind: &str| events.iter().filter(|e| e.kind == kind).count();
    (count(INTERRUPTED_KIND), count(TIMEOUT_KIND))
}

#[tokio::test]
async fn a_restart_interrupts_the_turn_and_expires_its_prompt_and_the_session_resumes() {
    // Arrange
    let (home, work) = (short_dir(), short_dir());
    let session = kill_daemon_mid_turn(home.path(), work.path()).await;
    let dirs = Dirs::from_base(home.path());
    let left_socket = dirs.socket_path().exists();

    // Act
    let _restarted = start_daemon(home.path());
    let client = connect(home.path()).await;
    let mut events = client.subscribe(&session, Some(0)).await.unwrap();
    let interrupted = next_matching(&mut events, |method, params| {
        method == EVENT_METHOD && params["kind"] == INTERRUPTED_KIND
    })
    .await;
    let expired = next_matching(&mut events, |method, params| {
        method == EVENT_METHOD && params["kind"] == TIMEOUT_KIND
    })
    .await;
    let resumed: SessionResumeResult = client
        .request(
            SESSION_RESUME,
            &SessionResumeParams {
                session_id: session.clone(),
            },
        )
        .await
        .unwrap();
    let status: DaemonStatusResult = client
        .request(DAEMON_STATUS, &DaemonStatusParams {})
        .await
        .unwrap();
    let sent: SessionSendResult = client
        .request(
            SESSION_SEND,
            &SessionSendParams {
                session_id: session.clone(),
                text: PERMISSION_PROMPT.to_owned(),
                max_turns: None,
                token_cap: None,
            },
        )
        .await
        .unwrap();
    let asked = next_matching(&mut events, |method, _| method == MID_TURN_PROMPT).await;

    // Assert
    assert!(left_socket, "the killed daemon should leave its socket");
    assert_eq!(interrupted["session_id"], session.as_str());
    assert_eq!(expired["session_id"], session.as_str());
    assert_eq!(resumed.session.id, session);
    assert_eq!(status.active_turns, 0);
    assert_eq!(asked["session_id"], session.as_str());
    assert_ne!(sent.turn_id.as_str(), "");
}

#[tokio::test]
async fn recovering_twice_adds_no_further_events() {
    // Arrange
    let (home, work) = (short_dir(), short_dir());
    let session = kill_daemon_mid_turn(home.path(), work.path()).await;
    let mut first = start_daemon(home.path());
    drop(connect(home.path()).await);
    sigkill(&mut first);
    let after_first = stored_counts(home.path(), &session);

    // Act
    let mut second = start_daemon(home.path());
    drop(connect(home.path()).await);
    sigkill(&mut second);

    // Assert
    assert_eq!(after_first, (1, 1));
    assert_eq!(stored_counts(home.path(), &session), after_first);
}

#[tokio::test]
async fn a_second_daemon_leaves_a_live_daemons_socket_alone() {
    // Arrange
    let home = short_dir();
    let _live = start_daemon(home.path());
    drop(connect(home.path()).await);
    let mut rival = spawn_rival(home.path());

    // Act
    let status = eventually("the rival daemon giving up", || {
        rival.child.try_wait().unwrap()
    })
    .await;

    let mut message = String::new();
    rival
        .child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut message)
        .unwrap();

    // Assert
    assert!(!status.success());
    assert!(message.contains(ALREADY_RUNNING_MESSAGE), "{message}");
    let still_serving = tokio::time::timeout(SERVING_PROBE, connect(home.path())).await;
    assert!(still_serving.is_ok());
}
