// SPDX-License-Identifier: AGPL-3.0-only

//! What happens to backend children when the real `coda daemon` dies with SIGKILL.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use coda_config::config::Dirs;
use coda_protocol::methods::{
    SESSION_CREATE, SESSION_SEND, SessionCreateParams, SessionCreateResult, SessionSendParams,
};
use coda_protocol::types::{BackendKind, Mode, SessionId};
use common::{Daemon, POLL, WAIT, connect, short_dir, start_daemon};
use serde_json::{Value, json};

const PERMISSION_PROMPT: &str = "hello";
const GRANDCHILD_PROMPT: &str = "grandchild";
const GRANDCHILD_LOG: &str = "grandchildren";
const CONTROL_FILE: &str = "fake-claude-control.json";
/// Written by `fake-claude` once it is running, before it reads any input.
const STARTED_REPORT: &str = "started";

/// Starts a session whose backend `prompt` puts into a known waiting state and
/// returns it once the backend has written its pidfile.
async fn start_turn(daemon_home: &Path, work: &Path, prompt: &str) -> SessionId {
    let log = work.join(GRANDCHILD_LOG);
    std::fs::write(
        work.join(CONTROL_FILE),
        json!({ "pid_log": log, "report_path": work.join(STARTED_REPORT) }).to_string(),
    )
    .unwrap();
    let client = connect(daemon_home).await;
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
    let _: Value = client
        .request(
            SESSION_SEND,
            &SessionSendParams {
                session_id: session.clone(),
                text: prompt.to_owned(),
                max_turns: None,
                token_cap: None,
            },
        )
        .await
        .unwrap();
    session
}

fn backend_pid(home: &Path, session: &SessionId) -> Option<u32> {
    let path = Dirs::from_base(home)
        .run_dir()
        .join(format!("{}.pid", session.as_str()));
    let record: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    u32::try_from(record["pid"].as_u64()?).ok()
}

fn grandchild_pid(work: &Path) -> Option<u32> {
    std::fs::read_to_string(work.join(GRANDCHILD_LOG))
        .ok()?
        .lines()
        .next()?
        .parse()
        .ok()
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Polls `probe` until it returns a value, failing the test after [`WAIT`].
async fn eventually<T>(what: &str, probe: impl Fn() -> Option<T>) -> T {
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

fn sigkill(daemon: &mut Daemon) {
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
}

#[tokio::test]
async fn a_backend_waiting_on_stdin_exits_by_itself_when_the_daemon_is_killed() {
    // Arrange
    let (home, work) = (short_dir(), short_dir());
    let mut daemon = start_daemon(home.path());
    let session = start_turn(home.path(), work.path(), PERMISSION_PROMPT).await;
    let backend = eventually("the backend's pidfile", || {
        backend_pid(home.path(), &session)
    })
    .await;
    eventually("the backend running", || {
        (work.path().join(STARTED_REPORT).exists() && alive(backend)).then_some(())
    })
    .await;

    // Act
    sigkill(&mut daemon);

    // Assert
    eventually("the backend noticing the closed pipe", || {
        (!alive(backend)).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_restarted_daemon_reaps_the_group_a_killed_daemon_left_behind() {
    // Arrange
    let (home, work) = (short_dir(), short_dir());
    let mut first = start_daemon(home.path());
    let session = start_turn(home.path(), work.path(), GRANDCHILD_PROMPT).await;
    let backend = eventually("the backend's pidfile", || {
        backend_pid(home.path(), &session)
    })
    .await;
    let grandchild = eventually("the backend's grandchild", || grandchild_pid(work.path())).await;
    sigkill(&mut first);
    let orphaned = alive(backend) && alive(grandchild);

    // Act
    let _second = start_daemon(home.path());

    // Assert
    assert!(orphaned, "the killed daemon should leave its group running");
    eventually("the reaper killing the group", || {
        (!alive(backend) && !alive(grandchild)).then_some(())
    })
    .await;
    let pidfile = Dirs::from_base(home.path())
        .run_dir()
        .join(format!("{}.pid", session.as_str()));
    eventually("the reaper removing the pidfile", || {
        (!pidfile.exists()).then_some(())
    })
    .await;
}
