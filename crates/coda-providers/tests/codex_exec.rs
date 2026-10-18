// SPDX-License-Identifier: AGPL-3.0-only

//! The Codex exec adapter driven against the scripted `fake-codex` binary.
#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use coda_protocol::events::Event;
use coda_protocol::types::{SessionId, TokenUsage, TurnId};
use coda_providers::claude_code::Diagnostic;
use coda_providers::claude_session::{ENV_ALLOWLIST, TurnOutcome};
use coda_providers::codex_exec::{CodexError, CodexExecSession, CodexSpec, SandboxMode};
use serde_json::Value;

const TURN_TIMEOUT: Duration = Duration::from_secs(30);
const GONE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const CONTROL_FILE: &str = "fake-codex-control.json";
const REPORT_FILE: &str = "report.json";
const PID_FILE: &str = "grandchild.pid";
const FAKE_THREAD: &str = "fake-thread-1";
const HOSTILE_ID: &str = "--dangerously-bypass-approvals-and-sandbox";

struct Fixture {
    cwd: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            cwd: tempfile::tempdir().unwrap(),
        };
        let control = serde_json::json!({
            "report_path": fixture.cwd.path().join(REPORT_FILE),
            "pid_path": fixture.cwd.path().join(PID_FILE),
        });
        fs::write(fixture.cwd.path().join(CONTROL_FILE), control.to_string()).unwrap();
        fixture
    }

    fn spec(&self, parent_env: &[(&str, &str)]) -> CodexSpec {
        CodexSpec {
            binary: coda_testkit::paths::fake_codex_path().unwrap(),
            cwd: self.cwd.path().to_path_buf(),
            session_id: SessionId::new("s-1"),
            resume: None,
            sandbox: SandboxMode::default(),
            parent_env: parent_env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
        }
    }

    fn session(&self) -> CodexExecSession {
        CodexExecSession::new(self.spec(&[]))
    }

    fn report(&self) -> Value {
        let raw = fs::read_to_string(self.cwd.path().join(REPORT_FILE)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn pid_from(&self, file: &str, report: bool) -> rustix::process::Pid {
        let raw = if report {
            self.report()["pid"].to_string()
        } else {
            fs::read_to_string(self.cwd.path().join(file)).unwrap()
        };
        rustix::process::Pid::from_raw(raw.trim().parse().unwrap()).unwrap()
    }
}

async fn turn(session: &mut CodexExecSession, text: &str) -> (TurnOutcome, Vec<Event>) {
    let mut events = Vec::new();
    let outcome = tokio::time::timeout(
        TURN_TIMEOUT,
        session.run_turn(&TurnId::new("t-1"), text, &mut |event| events.push(event)),
    )
    .await
    .expect("turn did not finish in time")
    .unwrap();
    (outcome, events)
}

fn process_exists(pid: rustix::process::Pid) -> bool {
    rustix::process::test_kill_process(pid).is_ok()
}

async fn assert_gone_soon(pid: rustix::process::Pid) {
    let deadline = tokio::time::Instant::now() + GONE_TIMEOUT;
    while process_exists(pid) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "process {pid:?} is still alive"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn wait_for_file(file: PathBuf) {
    let deadline = tokio::time::Instant::now() + GONE_TIMEOUT;
    while !file.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[tokio::test]
async fn a_turn_maps_codex_events_to_coda_events() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = fixture.session();

    // Act
    let (outcome, events) = turn(&mut session, "say hi").await;

    // Assert
    let (session_id, turn_id) = (SessionId::new("s-1"), TurnId::new("t-1"));
    assert_eq!(outcome, TurnOutcome::Completed { is_error: false });
    assert_eq!(
        events,
        vec![
            Event::StreamDelta {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                text: "working on it".to_owned(),
            },
            Event::Usage {
                session_id: session_id.clone(),
                turn_id: turn_id.clone(),
                usage: TokenUsage {
                    input_tokens: 120,
                    output_tokens: 30,
                    cache_read_tokens: 100,
                    cache_write_tokens: 0,
                },
            },
            Event::Result {
                session_id,
                turn_id,
                is_error: false
            },
        ]
    );
    assert_eq!(session.session_id_from_backend(), Some(FAKE_THREAD));
}

#[tokio::test]
async fn the_prompt_goes_over_stdin_and_the_sandbox_widens_only_when_configured() {
    // Arrange
    let fixture = Fixture::new();
    let mut default_session = fixture.session();
    let mut wide_spec = fixture.spec(&[]);
    wide_spec.sandbox = SandboxMode::WorkspaceWrite;
    let mut wide_session = CodexExecSession::new(wide_spec);

    // Act
    turn(&mut default_session, "secret prompt text").await;
    let default_report = fixture.report();
    turn(&mut wide_session, "hi").await;
    let wide_sandbox = fixture.report()["sandbox"].clone();

    // Assert
    assert_eq!(default_report["prompt"], "secret prompt text");
    assert!(
        !default_report["args"]
            .to_string()
            .contains("secret prompt text")
    );
    assert_eq!(default_report["sandbox"], "read-only");
    assert_eq!(wide_sandbox, "workspace-write");
}

#[tokio::test]
async fn a_configured_or_reported_thread_id_is_resumed_on_the_next_turn() {
    // Arrange
    let fixture = Fixture::new();
    let mut spec = fixture.spec(&[]);
    spec.resume = Some("earlier-thread".to_owned());
    let mut configured = CodexExecSession::new(spec);
    let mut fresh = fixture.session();

    // Act
    turn(&mut configured, "first").await;
    let configured_first = fixture.report()["args"].to_string();
    turn(&mut fresh, "first").await;
    let fresh_first = fixture.report()["args"].to_string();
    turn(&mut fresh, "second").await;
    let fresh_second = fixture.report()["args"].to_string();

    // Assert
    assert!(configured_first.contains(r#""resume","earlier-thread""#));
    assert!(!fresh_first.contains("resume"));
    assert!(fresh_second.contains(&format!(r#""resume","{FAKE_THREAD}""#)));
}

#[tokio::test]
async fn the_child_gets_only_allowlisted_environment() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = CodexExecSession::new(
        fixture.spec(&[("OPENAI_API_KEY", "canary"), ("HOME", "/tmp/codex-home")]),
    );

    // Act
    turn(&mut session, "hi").await;

    // Assert
    let report = fixture.report();
    let names = report["env_names"].as_array().unwrap();
    assert!(
        names
            .iter()
            .all(|name| ENV_ALLOWLIST.contains(&name.as_str().unwrap()))
    );
    assert!(!names.contains(&Value::from("OPENAI_API_KEY")));
    assert_eq!(report["home"], "/tmp/codex-home");
}

#[tokio::test]
async fn a_thread_id_that_looks_like_a_flag_is_never_adopted_from_child_output() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = fixture.session();

    // Act
    turn(&mut session, "hostile").await;
    let adopted = session.session_id_from_backend().map(str::to_owned);
    turn(&mut session, "second").await;

    // Assert
    assert_eq!(adopted, None);
    assert!(!fixture.report()["args"].to_string().contains(HOSTILE_ID));
}

#[tokio::test]
async fn bad_lines_are_recorded_and_the_turn_completes() {
    // Arrange
    let unknown = Diagnostic::UnknownType {
        line: 2,
        kind: "mystery".to_owned(),
    };
    let cases = [
        (
            "noise",
            vec![Diagnostic::MalformedLine { line: 1 }, unknown],
        ),
        ("bigline", vec![Diagnostic::LineTooLong { line: 1 }]),
    ];

    for (prompt, expected) in cases {
        let fixture = Fixture::new();
        let mut session = fixture.session();

        // Act
        let (outcome, _) = turn(&mut session, prompt).await;

        // Assert
        assert_eq!(outcome, TurnOutcome::Completed { is_error: false });
        assert_eq!(session.diagnostics(), expected);
    }
}

#[tokio::test]
async fn a_turn_failed_event_ends_the_turn_as_an_error() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = fixture.session();

    // Act
    let (outcome, events) = turn(&mut session, "fail").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Completed { is_error: true });
    assert!(matches!(
        events.as_slice(),
        [Event::Result { is_error: true, .. }]
    ));
}

#[tokio::test]
async fn a_crash_mid_turn_is_reported_and_the_next_turn_starts_a_new_process() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = fixture.session();

    // Act
    let (outcome, events) = turn(&mut session, "crash").await;
    let (next, _) = turn(&mut session, "after").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Crashed { exit_code: Some(3) });
    assert_eq!(
        events,
        vec![Event::BackendCrashed {
            session_id: SessionId::new("s-1"),
            exit_code: Some(3)
        }]
    );
    assert_eq!(next, TurnOutcome::Completed { is_error: false });
}

#[tokio::test]
async fn cancelling_kills_the_process_group_and_leaves_no_zombie() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let pid_file = fixture.cwd.path().join(PID_FILE);
    let handle = session.cancel_handle();
    tokio::spawn(async move {
        wait_for_file(pid_file).await;
        handle.cancel();
    });

    // Act
    let (outcome, events) = turn(&mut session, "grandchild").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert!(matches!(events.as_slice(), [Event::TurnInterrupted { .. }]));
    assert_gone_soon(fixture.pid_from(PID_FILE, false)).await;
    assert_gone_soon(fixture.pid_from(REPORT_FILE, true)).await;
    assert_eq!(session.pid(), None);
}

#[tokio::test]
async fn dropping_the_session_kills_the_process_group() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = fixture.session();
    let pid_file = fixture.cwd.path().join(PID_FILE);
    let (turn_id, mut sink) = (TurnId::new("t-1"), |_event: Event| {});
    let started = tokio::select! {
        _ = session.run_turn(&turn_id, "grandchild", &mut sink) => false,
        () = wait_for_file(pid_file) => true,
    };
    assert!(started, "the turn ended before the grandchild started");
    let (grandchild, leader) = (
        fixture.pid_from(PID_FILE, false),
        fixture.pid_from("", true),
    );

    // Act
    drop(session);

    // Assert
    assert_gone_soon(grandchild).await;
    assert_gone_soon(leader).await;
}

#[tokio::test]
async fn cancelling_before_the_turn_starts_spawns_nothing() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = fixture.session();
    session.cancel_handle().cancel();

    // Act
    let (outcome, _) = turn(&mut session, "hi").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert!(!fixture.cwd.path().join(REPORT_FILE).exists());
}

#[tokio::test]
async fn a_session_that_cannot_run_a_turn_fails_with_a_specific_error() {
    // Arrange
    let fixture = Fixture::new();
    let mut closed = fixture.session();
    closed.shutdown().await.unwrap();
    let mut hostile_spec = fixture.spec(&[]);
    hostile_spec.resume = Some(HOSTILE_ID.to_owned());
    let mut missing_spec = fixture.spec(&[]);
    missing_spec.binary = fixture.cwd.path().join("no-such-codex");
    let sessions = [
        closed,
        CodexExecSession::new(hostile_spec),
        CodexExecSession::new(missing_spec),
    ];

    // Act
    let mut errors = Vec::new();
    for mut session in sessions {
        let result = session
            .run_turn(&TurnId::new("t-1"), "hi", &mut |_| {})
            .await;
        errors.push(result.unwrap_err());
    }

    // Assert
    assert!(matches!(errors[0], CodexError::Closed));
    assert!(matches!(errors[1], CodexError::InvalidThreadId));
    assert!(matches!(errors[2], CodexError::Spawn { .. }));
    assert!(errors[2].to_string().contains("no-such-codex"));
    assert!(!fixture.cwd.path().join(REPORT_FILE).exists());
}
