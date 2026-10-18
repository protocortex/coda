// SPDX-License-Identifier: AGPL-3.0-only

//! `coda run` against the real daemon binary, driven by a scripted faux provider.

mod common;

use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use coda_cli::Context;
use coda_client::Client;
use coda_config::config::Dirs;
use coda_config::timing::Timing;
use coda_protocol::methods::{
    SESSION_CANCEL, SESSION_LIST, SessionCancelParams, SessionCancelResult, SessionListParams,
    SessionListResult, SessionStatus, SessionSummary,
};
use coda_store::Store;
use common::{Outcome, WAIT, cli, context, short_dir};
use serde_json::{Value, json};
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_coda");
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

/// Stops the daemon a test started, even when an assertion fails first.
struct StopDaemon(String);

impl Drop for StopDaemon {
    fn drop(&mut self) {
        let status = Command::new(BIN)
            .arg("status")
            .env("CODA_HOME", &self.0)
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default();
        if let Some(pid) = status.lines().find_map(|line| line.strip_prefix("pid: ")) {
            let _ = Command::new("kill").arg(pid).stderr(Stdio::null()).status();
        }
    }
}

struct Rig {
    home: TempDir,
    work: TempDir,
    ctx: Context,
    _stop: StopDaemon,
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
        _stop: StopDaemon(home.path().display().to_string()),
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
    let stopper = async {
        let session = loop {
            if let Ok(client) = Client::connect(&socket, Timing::for_tests()).await {
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
                if let Some(running) = listed
                    .sessions
                    .iter()
                    .find(|s| s.status == SessionStatus::Running)
                {
                    break (client, running.id.clone());
                }
            }
            tokio::time::sleep(POLL).await;
        };
        let (client, id) = session;
        let cancelled: SessionCancelResult = client
            .request(SESSION_CANCEL, &SessionCancelParams { session_id: id })
            .await
            .unwrap();
        cancelled
    };

    // Act
    let (outcome, cancelled) = tokio::join!(run(&rig, &[]), tokio::time::timeout(WAIT, stopper));
    let cancelled = cancelled.expect("the run never showed as running");

    // Assert
    assert!(cancelled.cancelled);
    assert_eq!(outcome.code, ExitCode::from(4), "{}", outcome.err);
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
    let client = Client::connect(&socket, Timing::for_tests()).await.unwrap();
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
