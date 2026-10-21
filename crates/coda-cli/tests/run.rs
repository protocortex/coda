// SPDX-License-Identifier: AGPL-3.0-only

//! `coda run` against the real daemon binary, driven by a scripted faux provider.

mod common;

use std::process::{Child, Command, ExitCode, Stdio};
use std::time::Duration;

use coda_cli::Context;
use coda_client::Client;
use coda_config::config::Dirs;
use coda_protocol::methods::{
    SESSION_CANCEL, SESSION_LIST, SessionCancelParams, SessionCancelResult, SessionListParams,
    SessionListResult, SessionStatus, SessionSummary,
};
use coda_protocol::types::SessionId;
use coda_store::Store;
use common::{Outcome, StopDaemon, WAIT, cli, connect, context, short_dir, timing};
use serde_json::{Value, json};
use tempfile::TempDir;

const POLL: Duration = Duration::from_millis(20);

fn text_turn(text: &str) -> Value {
    json!({"events": [
        {"type": "text_delta", "text": text},
        {"type": "usage", "input_tokens": 11, "output_tokens": 7},
        {"type": "stop", "reason": "end_turn"},
    ]})
}

fn tool_turn(name: &str, arguments: &Value) -> Value {
    json!({"events": [
        {"type": "tool_call_start", "call_id": "c1", "name": name},
        {"type": "tool_call_arguments_delta", "call_id": "c1", "fragment": arguments.to_string()},
        {"type": "usage", "input_tokens": 5, "output_tokens": 3},
        {"type": "stop", "reason": "tool_use"},
    ]})
}

fn overflow_turn() -> Value {
    json!({"events": [], "error": {"class": "context_window_exceeded"}})
}

struct Rig {
    // First, so the daemon is stopped while its home directory still exists.
    _stop: StopDaemon,
    home: TempDir,
    work: TempDir,
    ctx: Context,
}

/// A home whose config enables the faux provider with `script`, plus `extra` config text.
fn rig(script: &[Value], extra: &str) -> Rig {
    let home = short_dir();
    let work = short_dir();
    let script_path = home.path().join("script.json");
    std::fs::write(&script_path, Value::Array(script.to_vec()).to_string()).unwrap();
    let config = format!(
        "{extra}\n[providers.faux]\nenabled = true\nscript = {:?}\n",
        script_path.display().to_string()
    );
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    let ctx = context(home.path());
    Rig {
        _stop: StopDaemon::new(home.path()),
        home,
        work,
        ctx,
    }
}

async fn run(rig: &Rig, flags: &[&str]) -> Outcome {
    let cwd = rig.work.path().display().to_string();
    let mut args = vec!["coda", "run", "--prompt", "hello", "--cwd", &cwd];
    args.extend_from_slice(flags);
    cli(&rig.ctx, &args, "").await
}

#[tokio::test]
async fn a_run_that_ends_its_turn_prints_the_answer_and_exits_zero() {
    // Arrange
    let rig = rig(&[text_turn("all done")], "");

    // Act
    let outcome = run(&rig, &[]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(outcome.out, "all done\n");
}

#[tokio::test]
async fn each_stop_condition_exits_with_its_own_code() {
    let looping = tool_turn("missing", &json!({}));
    let cases = [
        (
            "max_turns",
            vec![looping.clone(), looping.clone()],
            vec!["--max-turns", "1"],
            1,
        ),
        (
            "budget_exhausted",
            vec![looping.clone(), text_turn("x")],
            vec!["--token-cap", "1"],
            2,
        ),
        ("context_overflow", vec![overflow_turn()], vec![], 3),
    ];
    for (name, script, flags, code) in cases {
        // Arrange
        let rig = rig(&script, "");

        // Act
        let outcome = run(&rig, &flags).await;

        // Assert
        assert_eq!(
            outcome.code,
            ExitCode::from(code),
            "{name}: {}",
            outcome.err
        );
    }
}

/// Three tool rounds, so a run holds more messages than compaction keeps.
fn long_run() -> Vec<Value> {
    vec![tool_turn("missing", &json!({})); 3]
}

#[tokio::test]
async fn a_run_whose_summary_fails_exits_five_with_its_own_message() {
    // Arrange
    let mut script = long_run();
    script.extend([
        overflow_turn(),
        json!({"events": [], "error": {"class": "fatal", "message": "refused"}}),
    ]);
    let rig = rig(&script, "");

    // Act
    let outcome = run(&rig, &[]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(5), "{}", outcome.err);
    assert!(
        outcome.err.contains("could not be compacted"),
        "{}",
        outcome.err
    );
}

#[tokio::test]
async fn a_cancelled_run_exits_four() {
    // Arrange
    let sleeper = tool_turn("bash", &json!({"command": "sleep 30"}));
    let rig = rig(&[sleeper], "[permissions]\ngrants = [\"bash:sleep 30\"]\n");
    // Grants only apply inside a git repository.
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(rig.work.path())
            .status()
            .unwrap()
            .success()
    );
    let socket = Dirs::from_base(rig.home.path()).socket_path();
    // A turn whose backend is still starting lists as running but answers a
    // cancel with `cancelled: false`, so the cancel is repeated until accepted.
    let cancel_loop = async {
        loop {
            if let Ok(client) = Client::connect(&socket, timing()).await
                && let Some(id) = running_session(&client).await
                && cancel(&client, id).await.cancelled
            {
                return;
            }
            tokio::time::sleep(POLL).await;
        }
    };

    // Act
    let (outcome, in_time) = tokio::join!(run(&rig, &[]), tokio::time::timeout(WAIT, cancel_loop));

    // Assert
    in_time.expect("the run was never cancelled");
    assert_eq!(outcome.code, ExitCode::from(4), "{}", outcome.err);
}

/// Kills the child on drop, so a failed assertion never leaves `coda run` behind.
struct Reaper(Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn an_interrupt_while_the_prompt_is_being_sent_cancels_the_turn_and_exits_four() {
    // Arrange
    let sleeper = tool_turn("bash", &json!({"command": "sleep 30"}));
    let rig = rig(
        std::slice::from_ref(&sleeper),
        "[permissions]\ngrants = [\"bash:sleep 30\"]\n",
    );
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(rig.work.path())
            .status()
            .unwrap()
            .success()
    );
    // The daemon reads the script while it starts the send, and a named pipe
    // holds that read until the test writes to it.
    let script = rig.home.path().join("script.json");
    std::fs::remove_file(&script).unwrap();
    assert!(
        Command::new("mkfifo")
            .arg(&script)
            .status()
            .unwrap()
            .success()
    );
    let stdout = std::fs::File::create(rig.home.path().join("run.out")).unwrap();
    let stderr = std::fs::File::create(rig.home.path().join("run.err")).unwrap();
    let mut child = Reaper(
        Command::new(env!("CARGO_BIN_EXE_coda"))
            .args(["run", "--prompt", "hello", "--cwd"])
            .arg(rig.work.path())
            .env("CODA_HOME", rig.home.path())
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap(),
    );
    let client = connect(rig.home.path()).await;
    let session = tokio::time::timeout(WAIT, async {
        loop {
            if let Some(id) = running_session(&client).await {
                return id;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the send never reached the daemon");

    // Act
    let interrupted = Command::new("kill")
        .args(["-INT", &child.0.id().to_string()])
        .status()
        .unwrap();
    // Opening a named pipe for writing blocks until the daemon reads it, so the
    // write runs on its own thread and a missing reader fails the test.
    let (written, released) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = written.send(std::fs::write(
            &script,
            Value::Array(vec![sleeper]).to_string(),
        ));
    });
    let status = tokio::time::timeout(WAIT, async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("coda run did not exit after the interrupt");

    // Assert
    assert!(interrupted.success());
    released
        .recv_timeout(WAIT)
        .expect("the daemon never read the script")
        .unwrap();
    assert_eq!(status.code(), Some(4), "{status:?}");
    let store = Store::open(&Dirs::from_base(rig.home.path()).data_dir()).unwrap();
    let events = store.load_events(&session, 0, 1000).unwrap();
    // The cancel reached the daemon while the send was still held, so the model
    // never ran: only an interrupt handled during the send can do that.
    let kinds: Vec<_> = events.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["user_message", "turn_stopped", "result"]);
    let stopped = events
        .into_iter()
        .find(|e| e.kind == "turn_stopped")
        .expect("the turn recorded no stop condition");
    let body: Value = serde_json::from_str(&stopped.body).unwrap();
    assert_eq!(body["condition"], "cancelled");
}

async fn running_session(client: &Client) -> Option<SessionId> {
    let listed: SessionListResult = client
        .request(
            SESSION_LIST,
            &SessionListParams {
                cwd: None,
                limit: None,
            },
        )
        .await
        .ok()?;
    listed
        .sessions
        .into_iter()
        .find(|s| s.status == SessionStatus::Running)
        .map(|s| s.id)
}

async fn cancel(client: &Client, session_id: SessionId) -> SessionCancelResult {
    client
        .request(SESSION_CANCEL, &SessionCancelParams { session_id })
        .await
        .unwrap()
}

#[tokio::test]
async fn a_headless_run_denies_a_permission_prompt_and_carries_on() {
    // Arrange
    let touch = tool_turn("bash", &json!({"command": "touch ran.txt"}));
    let rig = rig(&[touch, text_turn("denied")], "");

    // Act
    let outcome = run(&rig, &[]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(outcome.out, "denied\n");
    assert!(!rig.work.path().join("ran.txt").exists());
    let session = only_session(&rig).await;
    let store = Store::open(&Dirs::from_base(rig.home.path()).data_dir()).unwrap();
    let decisions = store.list_decisions(&session.id).unwrap();
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].allowed);
}

async fn only_session(rig: &Rig) -> SessionSummary {
    let socket = Dirs::from_base(rig.home.path()).socket_path();
    let client = Client::connect(&socket, timing()).await.unwrap();
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
    assert_eq!(listed.sessions.len(), 1);
    listed.sessions.into_iter().next().unwrap()
}

#[tokio::test]
async fn a_run_without_a_prompt_fails_with_the_failure_code_not_a_run_code() {
    // Arrange
    let rig = rig(&[], "");

    // Act
    let outcome = cli(&rig.ctx, &["coda", "run"], "").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(70), "{}", outcome.err);
    assert!(outcome.err.contains("no prompt"), "{}", outcome.err);
}

#[tokio::test]
async fn the_prompt_is_read_from_input_when_the_flag_is_missing() {
    // Arrange
    let rig = rig(&[text_turn("heard")], "");
    let cwd = rig.work.path().display().to_string();

    // Act
    let outcome = cli(&rig.ctx, &["coda", "run", "--cwd", &cwd], "from stdin\n").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(outcome.out, "heard\n");
}

/// A home whose config enables the delegated Claude Code backend, played by `fake-claude`.
fn claude_rig(extra: &str) -> Rig {
    let home = short_dir();
    let work = short_dir();
    let fake = coda_testkit::paths::fake_claude_path().unwrap();
    let config = format!(
        "[providers.claude_code]\nenabled = true\nbinary = {:?}\n\n{extra}",
        fake.display().to_string()
    );
    std::fs::write(home.path().join("config.toml"), config).unwrap();
    let ctx = context(home.path());
    Rig {
        _stop: StopDaemon::new(home.path()),
        home,
        work,
        ctx,
    }
}

async fn run_prompt(rig: &Rig, prompt: &str) -> Outcome {
    let cwd = rig.work.path().display().to_string();
    cli(
        &rig.ctx,
        &["coda", "run", "--prompt", prompt, "--cwd", &cwd],
        "",
    )
    .await
}

#[tokio::test]
async fn a_delegated_backend_that_keeps_crashing_ends_the_run_with_exit_five() {
    // Arrange
    let rig = claude_rig("[timing]\nrestart_backoff_ms = 10\n");

    // Act
    let outcome = run_prompt(&rig, "please crash").await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(5), "{}", outcome.err);
    assert!(outcome.err.contains("coda:"), "{}", outcome.err);
}

#[tokio::test]
async fn an_interrupted_delegated_run_exits_four() {
    // Arrange
    let rig = claude_rig("");
    let stdout = std::fs::File::create(rig.home.path().join("run.out")).unwrap();
    let stderr = std::fs::File::create(rig.home.path().join("run.err")).unwrap();
    let mut child = Reaper(
        Command::new(env!("CARGO_BIN_EXE_coda"))
            .args(["run", "--prompt", "please stall", "--cwd"])
            .arg(rig.work.path())
            .env("CODA_HOME", rig.home.path())
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap(),
    );
    let client = connect(rig.home.path()).await;
    tokio::time::timeout(WAIT, async {
        while running_session(&client).await.is_none() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the send never reached the daemon");

    // Act
    let interrupted = Command::new("kill")
        .args(["-INT", &child.0.id().to_string()])
        .status()
        .unwrap();
    let status = tokio::time::timeout(WAIT, async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("coda run did not exit after the interrupt");

    // Assert
    assert!(interrupted.success());
    assert_eq!(status.code(), Some(4), "{status:?}");
}
