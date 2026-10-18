// SPDX-License-Identifier: AGPL-3.0-only

//! The Claude Code adapter driven against the scripted `fake-claude` binary.
#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use coda_protocol::events::Event;
use coda_protocol::types::{PermissionDecision, RequestId, SessionId, TurnId};
use coda_providers::claude_code::Diagnostic;
use coda_providers::claude_session::{
    AdapterError, CancelHandle, ClaudeSession, SessionSpec, TurnOutcome,
};
use coda_providers::gate::PermissionGate;
use coda_providers::tool_request::{Tool, ToolRequest};
use serde_json::Value;

const TURN_TIMEOUT: Duration = Duration::from_secs(30);
const GONE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const CONTROL_FILE: &str = "fake-claude-control.json";
const REPORT_FILE: &str = "report.json";
const PID_FILE: &str = "grandchild.pid";
const FLOODED_MESSAGE_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "macos")]
const O_NONBLOCK: i32 = 0x0004;
#[cfg(not(target_os = "macos"))]
const O_NONBLOCK: i32 = 0x0800;

struct FixedGate {
    decision: PermissionDecision,
    seen: Mutex<Vec<ToolRequest>>,
}

impl FixedGate {
    fn new(decision: PermissionDecision) -> Self {
        Self {
            decision,
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl PermissionGate for FixedGate {
    fn request(&self, request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        self.seen.lock().unwrap().push(request.clone());
        std::future::ready(self.decision)
    }
}

struct PendingGate {
    cancel_on_request: Option<CancelHandle>,
}

impl PermissionGate for PendingGate {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        if let Some(handle) = &self.cancel_on_request {
            handle.cancel();
        }
        std::future::pending()
    }
}

struct Fixture {
    cwd: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            cwd: tempfile::tempdir().unwrap(),
        };
        fixture.control(false);
        fixture
    }

    fn control(&self, flood: bool) {
        let control = serde_json::json!({
            "report_path": self.cwd.path().join(REPORT_FILE),
            "pid_path": self.cwd.path().join(PID_FILE),
            "flood": flood,
        });
        fs::write(self.cwd.path().join(CONTROL_FILE), control.to_string()).unwrap();
    }

    fn grandchild_pid(&self) -> rustix::process::Pid {
        let raw = fs::read_to_string(self.cwd.path().join(PID_FILE)).unwrap();
        rustix::process::Pid::from_raw(raw.trim().parse().unwrap()).unwrap()
    }

    fn spec(&self, parent_env: Vec<(OsString, OsString)>) -> SessionSpec {
        SessionSpec {
            binary: coda_testkit::paths::fake_claude_path().unwrap(),
            cwd: self.cwd.path().to_path_buf(),
            session_id: SessionId::new("s-1"),
            resume: None,
            parent_env,
        }
    }

    fn report(&self) -> Value {
        let raw = fs::read_to_string(self.cwd.path().join(REPORT_FILE)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }
}

fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).into(), (*v).into()))
        .collect()
}

async fn turn(
    session: &mut ClaudeSession,
    gate: &FixedGate,
    text: &str,
) -> (TurnOutcome, Vec<Event>) {
    let mut events = Vec::new();
    let outcome = tokio::time::timeout(
        TURN_TIMEOUT,
        session.run_turn(&TurnId::new("t-1"), text, gate, &mut |event| {
            events.push(event);
        }),
    )
    .await
    .expect("turn did not finish in time")
    .unwrap();
    (outcome, events)
}

#[tokio::test]
async fn an_allowed_request_is_approved_and_the_turn_completes() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);

    // Act
    let (outcome, events) = turn(&mut session, &gate, "run ls").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Completed { is_error: false });
    assert!(events.contains(&Event::Approve {
        session_id: SessionId::new("s-1"),
        request_id: RequestId::new("req-1"),
        tool: "bash".to_owned(),
    }));
    assert!(events.contains(&Event::StreamDelta {
        session_id: SessionId::new("s-1"),
        turn_id: TurnId::new("t-1"),
        text: "working".to_owned(),
    }));
    let seen = gate.seen.lock().unwrap();
    assert_eq!(seen[0].tool, Tool::Bash);
    assert_eq!(seen[0].command.as_deref(), Some("ls"));
    assert_eq!(seen[0].cwd, fixture.cwd.path());
    assert_eq!(session.session_id_from_backend(), Some("fake-session-1"));
}

#[tokio::test]
async fn a_denied_request_is_reported_and_the_backend_sees_the_denial() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::Deny);

    // Act
    let (outcome, events) = turn(&mut session, &gate, "run ls").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Completed { is_error: false });
    assert!(events.iter().any(|e| matches!(e, Event::Deny { .. })));
    assert!(!events.iter().any(|e| matches!(e, Event::Approve { .. })));
}

#[tokio::test]
async fn a_process_exit_mid_turn_emits_backend_crashed() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);

    // Act
    let (outcome, events) = turn(&mut session, &gate, "please crash").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Crashed { exit_code: Some(3) });
    assert!(events.contains(&Event::BackendCrashed {
        session_id: SessionId::new("s-1"),
        exit_code: Some(3),
        will_restart: false,
    }));
}

#[tokio::test]
async fn malformed_and_unknown_lines_are_recorded_and_the_turn_continues() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);

    // Act
    let (outcome, _events) = turn(&mut session, &gate, "make noise").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Completed { is_error: false });
    let diagnostics = session.diagnostics();
    assert!(
        diagnostics
            .iter()
            .any(|d| matches!(d, Diagnostic::MalformedLine { .. }))
    );
    assert!(
        diagnostics
            .iter()
            .any(|d| matches!(d, Diagnostic::UnknownType { kind, .. } if kind == "mystery"))
    );
}

#[tokio::test]
async fn the_child_gets_only_allowlisted_environment() {
    // Arrange
    let fixture = Fixture::new();
    let parent = env(&[
        ("ANTHROPIC_API_KEY", "sk-test"),
        ("CLAUDECODE", "1"),
        ("HOME", "/tmp/coda-home"),
    ]);
    let mut session = ClaudeSession::spawn(fixture.spec(parent)).unwrap();
    let gate = FixedGate::new(PermissionDecision::Deny);

    // Act
    turn(&mut session, &gate, "run ls").await;

    // Assert
    let report = fixture.report();
    assert_eq!(report["anthropic_api_key_present"], false);
    assert_eq!(report["claudecode_present"], false);
    assert_eq!(report["home"], "/tmp/coda-home");
}

#[tokio::test]
async fn the_proven_flags_are_passed_and_resume_adds_the_session_id() {
    // Arrange
    let fixture = Fixture::new();
    let mut spec = fixture.spec(env(&[]));
    spec.resume = Some("sess-9".to_owned());
    let mut session = ClaudeSession::spawn(spec).unwrap();
    let gate = FixedGate::new(PermissionDecision::Deny);

    // Act
    turn(&mut session, &gate, "run ls").await;

    // Assert
    let args: Vec<String> = fixture.report()["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_owned())
        .collect();
    let expected = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompt-tool",
        "stdio",
        "--resume",
        "sess-9",
    ];
    assert_eq!(args, expected);
}

fn make_fifo(path: &Path) {
    let status = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(status.success());
}

/// A FIFO standing in for the credential file; a writer's byte lands in `reader`.
struct Canary {
    _home: tempfile::TempDir,
    reader: fs::File,
    home_path: String,
}

impl Canary {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let claude_dir = home.path().join(".claude");
        fs::create_dir(&claude_dir).unwrap();
        let canary = claude_dir.join(".credentials.json");
        make_fifo(&canary);
        let reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open(&canary)
            .unwrap();
        let home_path = home.path().to_str().unwrap().to_owned();
        Self {
            _home: home,
            reader,
            home_path,
        }
    }

    fn bytes_written_by_child(&mut self) -> usize {
        let mut buf = [0_u8; 8];
        self.reader.read(&mut buf).unwrap_or(0)
    }
}

#[tokio::test]
async fn a_child_that_opens_the_credential_file_is_observed_by_the_probe() {
    // Arrange
    let fixture = Fixture::new();
    let mut canary = Canary::new();
    let parent = env(&[("HOME", &canary.home_path)]);
    let mut session = ClaudeSession::spawn(fixture.spec(parent)).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);

    // Act
    turn(&mut session, &gate, "open-credentials").await;

    // Assert
    assert_eq!(canary.bytes_written_by_child(), 1);
}

#[tokio::test]
async fn a_credential_file_under_home_is_never_opened() {
    // Arrange
    let fixture = Fixture::new();
    let mut canary = Canary::new();
    let parent = env(&[("HOME", &canary.home_path)]);
    let mut session = ClaudeSession::spawn(fixture.spec(parent)).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);

    // Act
    let (outcome, events) = turn(&mut session, &gate, "run ls").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Completed { is_error: false });
    assert_eq!(canary.bytes_written_by_child(), 0);
    assert!(!format!("{events:?}").contains(".credentials"));
}

#[tokio::test]
async fn cancelling_before_the_first_output_ends_the_turn_cleanly() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);
    session.cancel_handle().cancel();

    // Act
    let (outcome, events) = turn(&mut session, &gate, "run ls").await;
    let shutdown = session.shutdown().await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert!(events.contains(&Event::TurnInterrupted {
        session_id: SessionId::new("s-1"),
        turn_id: TurnId::new("t-1"),
    }));
    assert!(shutdown.is_ok());
}

#[tokio::test]
async fn cancelling_after_completion_and_twice_is_harmless() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);
    turn(&mut session, &gate, "run ls").await;
    let handle = session.cancel_handle();

    // Act
    handle.cancel();
    handle.cancel();
    let first = session.shutdown().await;
    let second = session.shutdown().await;

    // Assert
    assert!(first.is_ok());
    assert!(second.is_ok());
}

#[tokio::test]
async fn a_turn_on_a_shut_down_session_is_refused() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);
    session.shutdown().await.unwrap();

    // Act
    let result = session
        .run_turn(&TurnId::new("t-2"), "run ls", &gate, &mut |_| {})
        .await;

    // Assert
    assert!(matches!(result, Err(AdapterError::Closed)));
}

#[tokio::test]
async fn a_response_for_an_unknown_request_id_is_rejected_without_effect() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);

    // Act
    let result = session
        .answer_permission("nope", PermissionDecision::AllowOnce)
        .await;
    let (outcome, _events) = turn(&mut session, &gate, "run ls").await;

    // Assert
    assert!(matches!(
        result,
        Err(AdapterError::UnknownRequest { request_id }) if request_id == "nope"
    ));
    assert_eq!(outcome, TurnOutcome::Completed { is_error: false });
}

#[tokio::test]
async fn a_missing_binary_fails_to_spawn_with_its_path() {
    // Arrange
    let fixture = Fixture::new();
    let mut spec = fixture.spec(env(&[]));
    spec.binary = fixture.cwd.path().join("no-such-claude");

    // Act
    let result = ClaudeSession::spawn(spec);

    // Assert
    assert!(matches!(result, Err(AdapterError::Spawn { .. })));
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

async fn cancelled_turn(session: &mut ClaudeSession, text: &str) -> (TurnOutcome, Vec<Event>) {
    let handle = session.cancel_handle();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);
    let mut events = Vec::new();
    let outcome = tokio::time::timeout(
        TURN_TIMEOUT,
        session.run_turn(&TurnId::new("t-1"), text, &gate, &mut |event| {
            if matches!(event, Event::StreamDelta { .. }) {
                handle.cancel();
            }
            events.push(event);
        }),
    )
    .await
    .expect("cancel did not end the turn")
    .unwrap();
    (outcome, events)
}

#[tokio::test]
async fn cancelling_mid_stream_stops_a_child_that_produces_no_more_output() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();

    // Act
    let (outcome, events) = cancelled_turn(&mut session, "stall").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnInterrupted { .. }))
    );
    assert!(session.shutdown().await.is_ok());
}

#[tokio::test]
async fn cancelling_while_a_permission_request_is_pending_ends_the_turn() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = PendingGate {
        cancel_on_request: Some(session.cancel_handle()),
    };
    let mut events = Vec::new();

    // Act
    let outcome = tokio::time::timeout(
        TURN_TIMEOUT,
        session.run_turn(&TurnId::new("t-1"), "run ls", &gate, &mut |event| {
            events.push(event);
        }),
    )
    .await
    .expect("cancel did not end the turn")
    .unwrap();

    // Assert
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::TurnInterrupted { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Approve { .. } | Event::Deny { .. }))
    );
}

#[tokio::test]
async fn cancelling_kills_a_grandchild_in_the_process_group() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();

    // Act
    let (outcome, _events) = cancelled_turn(&mut session, "grandchild").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert_gone_soon(fixture.grandchild_pid()).await;
}

#[tokio::test]
async fn end_of_output_from_a_live_child_kills_the_group_and_does_not_hang() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);

    // Act
    let (outcome, events) = turn(&mut session, &gate, "orphan").await;
    let shutdown = session.shutdown().await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Crashed { exit_code: None });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BackendCrashed { .. }))
    );
    assert_gone_soon(fixture.grandchild_pid()).await;
    assert!(shutdown.is_ok());
}

#[tokio::test]
async fn dropping_a_session_kills_the_whole_process_group() {
    // Arrange
    let fixture = Fixture::new();
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);
    let started = tokio::sync::Notify::new();
    {
        let turn_id = TurnId::new("t-1");
        let mut notify = |_: Event| started.notify_one();
        let run = session.run_turn(&turn_id, "grandchild", &gate, &mut notify);

        // Act
        tokio::select! {
            _ = run => panic!("the stalled turn must not finish"),
            () = started.notified() => {}
        }
    }
    drop(session);

    // Assert
    assert_gone_soon(fixture.grandchild_pid()).await;
}

#[tokio::test]
async fn a_large_message_does_not_deadlock_against_a_child_flooding_its_output() {
    // Arrange
    let fixture = Fixture::new();
    fixture.control(true);
    let mut session = ClaudeSession::spawn(fixture.spec(env(&[]))).unwrap();
    let gate = FixedGate::new(PermissionDecision::AllowOnce);
    let message = "a".repeat(FLOODED_MESSAGE_BYTES);

    // Act
    let (outcome, _events) = turn(&mut session, &gate, &message).await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Completed { is_error: false });
    assert_ne!(session.diagnostics().len(), 0);
}
