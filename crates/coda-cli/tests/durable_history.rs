// SPDX-License-Identifier: AGPL-3.0-only

//! A conversation survives the death of the daemon that held it.

mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use coda_client::Client;
use coda_config::config::Dirs;
use coda_protocol::methods::{
    SESSION_CREATE, SESSION_LIST, SESSION_SEND, SessionCreateParams, SessionCreateResult,
    SessionListParams, SessionListResult, SessionSendParams, SessionStatus,
};
use coda_protocol::types::{BackendKind, Mode, SessionId};
use common::{Daemon, POLL, StopDaemon, WAIT, cli, connect, context, short_dir};
use serde_json::{Value, json};

const REQUEST_LOG: &str = "requests.jsonl";
const PID_PREFIX: &str = "pid: ";

fn text_turn(text: &str) -> Value {
    json!({"events": [
        {"type": "text_delta", "text": text},
        {"type": "stop", "reason": "end_turn"},
    ]})
}

/// Starts `coda daemon` in `home`, whose config serves the faux script and logs its requests.
fn spawn_daemon(home: &Path) -> Daemon {
    let config = format!(
        "[providers.faux]\nenabled = true\nscript = {:?}\nrequest_log = {:?}\n",
        home.join("script.json").display().to_string(),
        home.join(REQUEST_LOG).display().to_string(),
    );
    std::fs::write(home.join("config.toml"), config).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_coda"))
        .arg("daemon")
        .env("CODA_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("coda binary should start");
    Daemon {
        child,
        run_dir: Dirs::from_base(home).run_dir(),
    }
}

async fn daemon_pid(home: &Path) -> u32 {
    let outcome = cli(&context(home), &["coda", "status"], "").await;
    outcome
        .out
        .lines()
        .find_map(|line| line.strip_prefix(PID_PREFIX))
        .unwrap_or_else(|| panic!("no pid in the status output: {}", outcome.out))
        .parse()
        .unwrap()
}

async fn send(client: &Client, session: &SessionId, text: &str) {
    let params = SessionSendParams {
        session_id: session.clone(),
        text: text.to_owned(),
        max_turns: None,
        token_cap: None,
    };
    let _: Value = client.request(SESSION_SEND, &params).await.unwrap();
}

fn logged_requests(home: &Path) -> Vec<Value> {
    let log = std::fs::read_to_string(home.join(REQUEST_LOG)).unwrap_or_default();
    log.lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Waits until `requests` requests were logged and the session has no turn running.
async fn wait_for_turn(client: &Client, home: &Path, requests: usize) {
    tokio::time::timeout(WAIT, async {
        loop {
            let listed: SessionListResult = client
                .request(
                    SESSION_LIST,
                    &SessionListParams {
                        cwd: None,
                        limit: None,
                    },
                )
                .await
                .unwrap();
            let idle = listed
                .sessions
                .iter()
                .all(|s| s.status == SessionStatus::Idle);
            if idle && logged_requests(home).len() >= requests {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the turn did not finish in time");
}

fn tool_turn() -> Value {
    json!({"events": [
        {"type": "tool_call_start", "call_id": "c1", "name": "read"},
        {"type": "tool_call_arguments_delta", "call_id": "c1", "fragment": "{\"path\":\"a.txt\"}"},
        {"type": "stop", "reason": "tool_use"},
    ]})
}

/// What the restarted daemon's provider was sent, and the two daemons' PIDs.
struct Restarted {
    requests: Vec<Value>,
    pids: (u32, u32),
}

/// Sends "first" (taking `first_requests` model calls), kills the daemon with
/// SIGKILL, starts another and sends "second".
async fn restart_between_sends(script: Vec<Value>, first_requests: usize) -> Restarted {
    let (home, work) = (short_dir(), short_dir());
    let _stop = StopDaemon::new(home.path());
    std::fs::write(work.path().join("a.txt"), "one needle here\n").unwrap();
    std::fs::write(
        home.path().join("script.json"),
        Value::Array(script).to_string(),
    )
    .unwrap();
    let mut first = spawn_daemon(home.path());
    let client = connect(home.path()).await;
    let created: SessionCreateResult = client
        .request(
            SESSION_CREATE,
            &SessionCreateParams {
                backend: BackendKind::Faux,
                cwd: work.path().display().to_string(),
                model: None,
                mode: Mode::Default,
            },
        )
        .await
        .unwrap();
    let session = created.session.id;
    send(&client, &session, "first").await;
    wait_for_turn(&client, home.path(), first_requests).await;
    let first_pid = daemon_pid(home.path()).await;

    first.child.kill().unwrap();
    first.child.wait().unwrap();
    drop(client);
    let _second = spawn_daemon(home.path());
    let client = connect(home.path()).await;
    let second_pid = daemon_pid(home.path()).await;
    send(&client, &session, "second").await;
    wait_for_turn(&client, home.path(), first_requests + 1).await;
    Restarted {
        requests: logged_requests(home.path()),
        pids: (first_pid, second_pid),
    }
}

#[tokio::test]
async fn a_daemon_started_after_a_kill_continues_the_conversation() {
    // Arrange
    let script = vec![text_turn("one"), text_turn("two")];

    // Act
    let restarted = restart_between_sends(script, 1).await;

    // Assert
    assert_ne!(
        restarted.pids.0, restarted.pids.1,
        "a new daemon serves the session"
    );
    assert_eq!(
        restarted.requests[1]["messages"],
        json!([
            {"role": "user", "text": "first"},
            {"role": "assistant", "text": "one", "tool_calls": []},
            {"role": "user", "text": "second"},
        ])
    );
}

#[tokio::test]
async fn a_daemon_started_after_a_kill_replays_the_tool_calls_and_results() {
    // Arrange
    let script = vec![tool_turn(), text_turn("done")];

    // Act
    let restarted = restart_between_sends(script, 2).await;

    // Assert
    assert_ne!(
        restarted.pids.0, restarted.pids.1,
        "a new daemon serves the session"
    );
    assert_eq!(
        restarted.requests[2]["messages"],
        json!([
            {"role": "user", "text": "first"},
            {"role": "assistant", "text": "", "tool_calls": [
                {"id": "c1", "name": "read", "arguments": "{\"path\":\"a.txt\"}"},
            ]},
            {"role": "tool", "call_id": "c1", "text": "one needle here\n", "is_error": false},
            {"role": "assistant", "text": "done", "tool_calls": []},
            {"role": "user", "text": "second"},
        ])
    );
}
