// SPDX-License-Identifier: AGPL-3.0-only

//! The Codex app-server adapter driven against the scripted `fake-codex` WebSocket mode.
//! The wire shapes are SYNTHETIC: written from the documented method names, not recorded.
#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use coda_protocol::events::Event;
use coda_protocol::types::{PermissionDecision, RequestId, SessionId, TokenUsage, TurnId};
use coda_providers::claude_session::{ENV_ALLOWLIST, TurnOutcome};
use coda_providers::codex_app_server::{AppServerError, AppServerSpec, CodexAppServerSession};
use coda_providers::codex_exec::SandboxMode;
use coda_providers::gate::PermissionGate;
use coda_providers::tool_request::{Tool, ToolRequest};
use serde_json::{Value, json};

const TURN_TIMEOUT: Duration = Duration::from_secs(30);
const GONE_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const CONTROL_FILE: &str = "fake-codex-control.json";
const REPORT_FILE: &str = "report.json";
const PID_FILE: &str = "grandchild.pid";
const UNIX_SOCKET_PATH_LIMIT: usize = 104;
/// Set by the LLVM profiling runtime inside a coverage-instrumented child on Linux, not by the
/// adapter.
const COVERAGE_RUNTIME_ENV: &[&str] = &["__LLVM_PROFILE_RT_INIT_ONCE"];

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

/// Holds the decision until the fake reports `key`, proving the adapter kept reading meanwhile.
struct WaitForReportGate {
    report: PathBuf,
    key: &'static str,
}

impl PermissionGate for WaitForReportGate {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        let (report, key) = (self.report.clone(), self.key);
        async move {
            let deadline = tokio::time::Instant::now() + GONE_TIMEOUT;
            while tokio::time::Instant::now() < deadline {
                let seen = fs::read_to_string(&report)
                    .ok()
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .is_some_and(|value| value[key] == true);
                if seen {
                    return PermissionDecision::AllowOnce;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
            PermissionDecision::Deny
        }
    }
}

/// Never answers, like a user who walked away.
struct PendingGate;

impl PermissionGate for PendingGate {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::pending()
    }
}

struct Fixture {
    cwd: tempfile::TempDir,
}

impl Fixture {
    fn new(control: &Value) -> Self {
        let fixture = Self {
            cwd: tempfile::tempdir().unwrap(),
        };
        let mut control = control.clone();
        control["report_path"] = json!(fixture.cwd.path().join(REPORT_FILE));
        control["pid_path"] = json!(fixture.cwd.path().join(PID_FILE));
        fs::write(fixture.cwd.path().join(CONTROL_FILE), control.to_string()).unwrap();
        fixture
    }

    fn spec(&self, verified: &[&str], parent_env: &[(&str, &str)]) -> AppServerSpec {
        AppServerSpec {
            binary: coda_testkit::paths::fake_codex_path().unwrap(),
            cwd: self.cwd.path().to_path_buf(),
            session_id: SessionId::new("s-1"),
            sandbox: SandboxMode::default(),
            ephemeral: true,
            verified_versions: verified.iter().map(|v| (*v).to_owned()).collect(),
            parent_env: parent_env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                .collect(),
        }
    }

    async fn session(&self) -> CodexAppServerSession {
        self.start(&["0.0.0-fake"], &[]).await.unwrap()
    }

    async fn start(
        &self,
        verified: &[&str],
        parent_env: &[(&str, &str)],
    ) -> Result<CodexAppServerSession, AppServerError> {
        let start = CodexAppServerSession::start(self.spec(verified, parent_env), None);
        tokio::time::timeout(TURN_TIMEOUT, start)
            .await
            .expect("start did not finish in time")
    }

    fn report(&self) -> Value {
        let raw = fs::read_to_string(self.cwd.path().join(REPORT_FILE)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn pid(&self, from_report: bool) -> rustix::process::Pid {
        let raw = if from_report {
            self.report()["pid"].to_string()
        } else {
            fs::read_to_string(self.cwd.path().join(PID_FILE)).unwrap()
        };
        rustix::process::Pid::from_raw(raw.trim().parse().unwrap()).unwrap()
    }
}

async fn try_turn(
    session: &mut CodexAppServerSession,
    gate: &FixedGate,
    text: &str,
) -> (Result<TurnOutcome, AppServerError>, Vec<Event>) {
    let mut events = Vec::new();
    let result = tokio::time::timeout(
        TURN_TIMEOUT,
        session.run_turn(&TurnId::new("t-1"), text, gate, &mut |e| events.push(e)),
    )
    .await
    .expect("turn did not finish in time");
    (result, events)
}

async fn turn(session: &mut CodexAppServerSession, text: &str) -> (TurnOutcome, Vec<Event>) {
    let gate = FixedGate::new(PermissionDecision::Deny);
    let (result, events) = try_turn(session, &gate, text).await;
    (result.unwrap(), events)
}

fn is_deltas(events: &[Event], expected: &str) -> bool {
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            Event::StreamDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    text == expected
}

async fn assert_gone_soon(pid: rustix::process::Pid) {
    let deadline = tokio::time::Instant::now() + GONE_TIMEOUT;
    while rustix::process::test_kill_process(pid).is_ok() {
        assert!(tokio::time::Instant::now() < deadline, "{pid:?} is alive");
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[tokio::test]
async fn a_turn_maps_app_server_notifications_to_coda_events() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;

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
    let params = &fixture.report()["thread_start"];
    assert_eq!(params["sandbox"], "read-only");
    assert_eq!(params["ephemeral"], true);
    assert_eq!(params["cwd"], json!(fixture.cwd.path()));
}

#[tokio::test]
async fn a_declined_command_approval_answers_decline_and_the_turn_reports_it() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;
    let gate = FixedGate::new(PermissionDecision::Deny);

    // Act
    let (result, events) = try_turn(&mut session, &gate, "approve-command").await;

    // Assert
    assert_eq!(result.unwrap(), TurnOutcome::Completed { is_error: false });
    assert_eq!(
        fixture.report()["approval_response"],
        json!({"decision": "decline"})
    );
    assert!(events.contains(&Event::Deny {
        session_id: SessionId::new("s-1"),
        request_id: RequestId::new("approval-1"),
        tool: "bash".to_owned(),
    }));
    let seen = gate.seen.lock().unwrap();
    assert_eq!(seen[0].tool, Tool::Bash);
    assert_eq!(seen[0].command.as_deref(), Some("rm -rf build"));
}

#[tokio::test]
async fn an_accepted_file_change_approval_uses_the_synthetic_accept_shape_and_item_paths() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;
    let gate = FixedGate::new(PermissionDecision::AllowOnce);

    // Act
    let (result, events) = try_turn(&mut session, &gate, "approve-file").await;

    // Assert
    assert_eq!(result.unwrap(), TurnOutcome::Completed { is_error: false });
    // SYNTHETIC: the live accept value is not recorded yet.
    assert_eq!(
        fixture.report()["approval_response"],
        json!({"decision": "accept"})
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Approve { tool, .. } if tool == "edit"))
    );
    let seen = gate.seen.lock().unwrap();
    assert_eq!(seen[0].tool, Tool::Edit);
    assert_eq!(
        seen[0].target_paths,
        vec![
            fixture.cwd.path().join("src/a.rs"),
            fixture.cwd.path().join("src/b.rs")
        ]
    );
}

#[tokio::test]
async fn a_non_101_upgrade_response_fails_the_start() {
    // Arrange
    let fixture = Fixture::new(&json!({"upgrade_status": 400}));

    // Act
    let result = fixture.start(&[], &[]).await;

    // Assert
    assert!(matches!(
        result,
        Err(AppServerError::Upgrade { status: 400 })
    ));
}

#[tokio::test]
async fn control_frames_and_fragmented_messages_are_handled() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;

    // Act
    let (ping, _) = turn(&mut session, "ping").await;
    let (fragmented, events) = turn(&mut session, "fragment").await;

    // Assert
    assert_eq!(ping, TurnOutcome::Completed { is_error: false });
    assert_eq!(fixture.report()["pong_seen"], true);
    assert_eq!(fixture.report()["pong_payload"], "hb");
    assert_eq!(fragmented, TurnOutcome::Completed { is_error: false });
    assert!(is_deltas(&events, "fragmented text"));
}

#[tokio::test]
async fn a_close_frame_or_exit_mid_turn_is_a_crash() {
    for (prompt, exit_code) in [("closemid", 0), ("crash", 3)] {
        // Arrange
        let fixture = Fixture::new(&json!({}));
        let mut session = fixture.session().await;

        // Act
        let (outcome, events) = turn(&mut session, prompt).await;

        // Assert
        assert_eq!(
            outcome,
            TurnOutcome::Crashed {
                exit_code: Some(exit_code)
            }
        );
        assert_eq!(
            events,
            vec![Event::BackendCrashed {
                session_id: SessionId::new("s-1"),
                exit_code: Some(exit_code),
                will_restart: false
            }]
        );
    }
}

#[tokio::test]
async fn an_oversized_frame_is_refused_and_the_child_stopped() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;
    let gate = FixedGate::new(PermissionDecision::Deny);

    // Act
    let (result, _) = try_turn(&mut session, &gate, "bigframe").await;

    // Assert
    assert!(matches!(result, Err(AppServerError::Protocol(_))));
    assert_gone_soon(fixture.pid(true)).await;
}

#[tokio::test]
async fn a_version_outside_the_allowlist_emits_a_drift_warning() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let verified = fixture.start(&["0.0.0-fake"], &[]).await.unwrap();
    let drifted = fixture.start(&["9.9.9"], &[]).await.unwrap();

    // Act
    let warning = drifted.drift_warning().map(str::to_owned);

    // Assert
    assert_eq!(verified.drift_warning(), None);
    assert_eq!(drifted.server_version(), "0.0.0-fake");
    assert!(warning.unwrap().contains("0.0.0-fake"));
}

#[tokio::test]
async fn the_child_gets_only_allowlisted_environment() {
    // Arrange
    let fixture = Fixture::new(&json!({}));

    // Act
    let session = fixture
        .start(
            &[],
            &[("OPENAI_API_KEY", "canary"), ("HOME", "/tmp/codex-home")],
        )
        .await
        .unwrap();

    // Assert
    let report = fixture.report();
    let names = report["env_names"].as_array().unwrap();
    let unexpected: Vec<&str> = names
        .iter()
        .map(|name| name.as_str().unwrap())
        .filter(|name| !ENV_ALLOWLIST.contains(name) && !COVERAGE_RUNTIME_ENV.contains(name))
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected child env: {unexpected:?}"
    );
    assert_eq!(report["home"], "/tmp/codex-home");
    drop(session);
}

#[tokio::test]
async fn the_socket_lives_in_a_private_directory_with_a_short_path() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let session = fixture.session().await;

    // Act
    let socket = session.socket_path();
    let mode = fs::metadata(socket.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();

    // Assert
    assert_eq!(mode & 0o777, 0o700);
    assert!(socket.as_os_str().len() < UNIX_SOCKET_PATH_LIMIT);
    drop(session);
    assert!(!socket.exists());
}

#[tokio::test]
async fn cancelling_kills_the_process_group() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;
    let handle = session.cancel_handle();
    let pid_file = fixture.cwd.path().join(PID_FILE);
    tokio::spawn(async move {
        while !pid_file.exists() {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        handle.cancel();
    });

    // Act
    let (outcome, events) = turn(&mut session, "grandchild").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert!(matches!(events.as_slice(), [Event::TurnInterrupted { .. }]));
    assert_gone_soon(fixture.pid(false)).await;
    assert_gone_soon(fixture.pid(true)).await;
    assert_eq!(session.pid(), None);
}

#[tokio::test]
async fn json_that_is_not_an_object_is_ignored() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;

    // Act
    let (outcome, events) = turn(&mut session, "nonobject").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Completed { is_error: false });
    assert!(is_deltas(&events, "working on it"));
}

#[tokio::test]
async fn an_approval_with_nothing_to_show_is_declined_without_asking_the_gate() {
    for prompt in ["approve-nocmd", "approve-unknown"] {
        // Arrange
        let fixture = Fixture::new(&json!({}));
        let mut session = fixture.session().await;
        let gate = FixedGate::new(PermissionDecision::AllowOnce);

        // Act
        let (result, events) = try_turn(&mut session, &gate, prompt).await;

        // Assert
        assert_eq!(result.unwrap(), TurnOutcome::Completed { is_error: false });
        assert_eq!(
            fixture.report()["approval_response"],
            json!({"decision": "decline"})
        );
        assert!(gate.seen.lock().unwrap().is_empty());
        assert!(events.iter().any(|e| matches!(e, Event::Deny { .. })));
    }
}

#[tokio::test]
async fn a_server_request_that_is_not_an_approval_gets_method_not_found() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;

    // Act
    let (outcome, _) = turn(&mut session, "other-request").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Completed { is_error: false });
    assert_eq!(fixture.report()["other_response"]["code"], -32601);
}

#[tokio::test]
async fn a_ping_during_a_pending_approval_is_answered() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;
    let gate = WaitForReportGate {
        report: fixture.cwd.path().join(REPORT_FILE),
        key: "pong_during_approval",
    };
    let mut events = Vec::new();

    // Act
    let result = tokio::time::timeout(
        TURN_TIMEOUT,
        session.run_turn(&TurnId::new("t-1"), "approve-heartbeat", &gate, &mut |e| {
            events.push(e);
        }),
    )
    .await
    .expect("turn did not finish in time");

    // Assert
    assert_eq!(result.unwrap(), TurnOutcome::Completed { is_error: false });
    assert_eq!(fixture.report()["pong_during_approval"], true);
    assert_eq!(
        fixture.report()["approval_response"],
        json!({"decision": "accept"})
    );
}

#[tokio::test]
async fn a_close_during_a_pending_approval_ends_the_turn_without_approving() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;
    let mut events = Vec::new();

    // Act
    let result = tokio::time::timeout(
        TURN_TIMEOUT,
        session.run_turn(
            &TurnId::new("t-1"),
            "approve-close",
            &PendingGate,
            &mut |e| {
                events.push(e);
            },
        ),
    )
    .await
    .expect("turn did not finish in time");

    // Assert
    assert_eq!(result.unwrap(), TurnOutcome::Crashed { exit_code: Some(0) });
    assert!(!events.iter().any(|e| matches!(e, Event::Approve { .. })));
    assert_eq!(fixture.report()["close_reply"], true);
}

#[tokio::test]
async fn a_server_close_is_answered_with_a_close_frame() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture.session().await;

    // Act
    let (outcome, _) = turn(&mut session, "closemid").await;

    // Assert
    assert_eq!(outcome, TurnOutcome::Crashed { exit_code: Some(0) });
    assert_eq!(fixture.report()["close_reply"], true);
}

#[tokio::test]
async fn a_thread_start_without_an_id_is_rejected() {
    // Arrange
    let fixture = Fixture::new(&json!({"no_thread_id": true}));

    // Act
    let result = fixture.start(&[], &[]).await;

    // Assert
    assert!(matches!(
        result,
        Err(AppServerError::Rejected {
            method: "thread/start"
        })
    ));
}

#[tokio::test]
async fn a_server_that_exits_before_listening_fails_the_start_quickly() {
    // Arrange
    let fixture = Fixture::new(&json!({"exit_early": true}));

    // Act
    let result = fixture.start(&[], &[]).await;

    // Assert
    // A start that kept waiting for the dead server would end as `StartTimeout`
    // instead, so the error tells the two apart without a tight wall-clock bound.
    assert!(matches!(
        result,
        Err(AppServerError::ExitedEarly { exit_code: Some(5) })
    ));
}
