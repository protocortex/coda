// SPDX-License-Identifier: AGPL-3.0-only

//! Codex backend selection and fallback, driven against the scripted `fake-codex` in both modes.
//! The wire shapes are SYNTHETIC: written from the documented names, not recorded.
#![cfg(unix)]

use std::fs;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coda_protocol::events::Event;
use coda_protocol::types::{BackendKind, PermissionDecision, SessionId, TurnId};
use coda_providers::claude_session::TurnOutcome;
use coda_providers::codex::{
    Carried, CodexLaunch, CodexSession, CodexSessionError, SandboxError, SpawnHook, resolve_sandbox,
};
use coda_providers::codex_app_server::AppServerError;
use coda_providers::codex_exec::{CodexError, SandboxMode};
use coda_providers::gate::PermissionGate;
use coda_providers::tool_request::ToolRequest;
use serde_json::{Value, json};

const TURN_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_FILE: &str = "fake-codex-control.json";
const REPORT_FILE: &str = "report.json";
const FAKE_VERSION: &str = "0.0.0-fake";

struct DenyGate;

impl PermissionGate for DenyGate {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::Deny)
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
        fs::write(fixture.cwd.path().join(CONTROL_FILE), control.to_string()).unwrap();
        fixture
    }

    fn launch(&self, verified: &[&str], sandbox: SandboxMode) -> CodexLaunch {
        CodexLaunch {
            binary: coda_testkit::paths::fake_codex_path().unwrap(),
            cwd: self.cwd.path().to_path_buf(),
            session_id: SessionId::new("s-1"),
            resume: None,
            sandbox,
            verified_versions: verified.iter().map(|v| (*v).to_owned()).collect(),
            binary_verified: verified.contains(&FAKE_VERSION),
            carried: Carried::Nothing,
            parent_env: Vec::new(),
        }
    }

    async fn start(
        &self,
        verified: &[&str],
        sandbox: SandboxMode,
    ) -> Result<CodexSession, CodexSessionError> {
        let start = CodexSession::start(self.launch(verified, sandbox), None);
        tokio::time::timeout(TURN_TIMEOUT, start)
            .await
            .expect("start did not finish in time")
    }

    fn report(&self) -> Value {
        let raw = fs::read_to_string(self.cwd.path().join(REPORT_FILE)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }
}

async fn turn(
    session: &mut CodexSession,
    text: &str,
) -> (Result<TurnOutcome, CodexSessionError>, Vec<Event>) {
    let mut events = Vec::new();
    let result = tokio::time::timeout(
        TURN_TIMEOUT,
        session.run_turn(&TurnId::new("t-1"), text, &DenyGate, &mut |e| {
            events.push(e);
        }),
    )
    .await
    .expect("turn did not finish in time");
    (result, events)
}

fn unavailable_reasons(events: &[Event]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ApprovalsUnavailable { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_unverified_binary_or_server_falls_back_to_read_only_exec_with_a_visible_reason() {
    // Arrange
    let cases = [
        (json!({"exit_early": true}), vec![], "not available"),
        (json!({"version": "9.9.9"}), vec![FAKE_VERSION], "9.9.9"),
    ];

    for (control, allowlist, reason) in cases {
        let fixture = Fixture::new(&control);
        let mut session = fixture
            .start(&allowlist, SandboxMode::default())
            .await
            .unwrap();

        // Act
        let (outcome, events) = turn(&mut session, "hello").await;

        // Assert
        assert_eq!(session.kind(), BackendKind::CodexExec);
        assert_eq!(outcome.unwrap(), TurnOutcome::Completed { is_error: false });
        let reasons = unavailable_reasons(&events);
        assert_eq!(reasons.len(), 1, "{control}");
        assert!(reasons[0].contains(reason), "{control}");
        assert_eq!(fixture.report()["sandbox"], "read-only");
    }
}

#[tokio::test]
async fn a_verified_binary_whose_app_server_fails_to_start_is_an_error_and_never_a_fallback() {
    // Arrange
    let exits = Fixture::new(&json!({"exit_early": true}));
    let missing = Fixture::new(&json!({}));
    let bad_upgrade = Fixture::new(&json!({"upgrade_status": 500}));
    let mut launch = missing.launch(&[FAKE_VERSION], SandboxMode::default());
    launch.binary = missing.cwd.path().join("no-such-codex");

    // Act
    let exited = exits.start(&[FAKE_VERSION], SandboxMode::default()).await;
    let spawned = CodexSession::start(launch, None).await;
    let refused = bad_upgrade
        .start(&[FAKE_VERSION], SandboxMode::default())
        .await;

    // Assert
    assert!(matches!(
        exited,
        Err(CodexSessionError::AppServer(
            AppServerError::ExitedEarly { .. }
        ))
    ));
    assert!(matches!(
        spawned,
        Err(CodexSessionError::AppServer(AppServerError::Spawn { .. }))
    ));
    assert!(matches!(
        refused,
        Err(CodexSessionError::AppServer(AppServerError::Upgrade {
            status: 500
        }))
    ));
}

#[tokio::test]
async fn a_verified_version_stays_on_the_app_server_without_a_fallback_event() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture
        .start(&[FAKE_VERSION], SandboxMode::default())
        .await
        .unwrap();

    // Act
    let (outcome, events) = turn(&mut session, "hello").await;

    // Assert
    assert_eq!(session.kind(), BackendKind::CodexAppServer);
    assert_eq!(outcome.unwrap(), TurnOutcome::Completed { is_error: false });
    assert_eq!(unavailable_reasons(&events).len(), 0);
    assert_eq!(fixture.report()["args"][0], "app-server");
}

#[tokio::test]
async fn an_app_server_crash_mid_turn_is_a_crash_event_and_never_a_fallback() {
    // Arrange
    let fixture = Fixture::new(&json!({}));
    let mut session = fixture
        .start(&[FAKE_VERSION], SandboxMode::default())
        .await
        .unwrap();

    // Act
    let (outcome, events) = turn(&mut session, "crash").await;
    let (next, _) = turn(&mut session, "after").await;

    // Assert
    assert!(matches!(outcome.unwrap(), TurnOutcome::Crashed { .. }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::BackendCrashed { .. }))
    );
    assert_eq!(unavailable_reasons(&events).len(), 0);
    assert_eq!(session.kind(), BackendKind::CodexAppServer);
    assert!(matches!(
        next,
        Err(CodexSessionError::AppServer(AppServerError::Closed))
    ));
    assert_eq!(fixture.report()["args"][0], "app-server");
}

#[tokio::test]
async fn an_opted_in_sandbox_reaches_the_app_server_but_the_fallback_stays_read_only() {
    // Arrange
    let verified = Fixture::new(&json!({}));
    let drifted = Fixture::new(&json!({"version": "9.9.9"}));

    // Act
    let mut server = verified
        .start(&[FAKE_VERSION], SandboxMode::WorkspaceWrite)
        .await
        .unwrap();
    let mut fallback = drifted
        .start(&[FAKE_VERSION], SandboxMode::WorkspaceWrite)
        .await
        .unwrap();
    turn(&mut server, "hello").await.0.unwrap();
    turn(&mut fallback, "hello").await.0.unwrap();

    // Assert
    assert_eq!(
        verified.report()["thread_start"]["sandbox"],
        "workspace-write"
    );
    assert_eq!(drifted.report()["sandbox"], "read-only");
}

#[test]
fn a_sandbox_wider_than_read_only_needs_an_explicit_opt_in() {
    // Arrange
    let refused = |name: &str| {
        Err(SandboxError::WiderRefused {
            requested: name.to_owned(),
        })
    };
    let cases = [
        (None, false, Ok(SandboxMode::ReadOnly)),
        (Some("read-only"), false, Ok(SandboxMode::ReadOnly)),
        (
            Some("workspace-write"),
            true,
            Ok(SandboxMode::WorkspaceWrite),
        ),
        (
            Some("danger-full-access"),
            true,
            Ok(SandboxMode::DangerFullAccess),
        ),
        (Some("workspace-write"), false, refused("workspace-write")),
        (
            Some("danger-full-access"),
            false,
            refused("danger-full-access"),
        ),
        (
            Some("everything"),
            true,
            Err(SandboxError::Unknown {
                requested: "everything".to_owned(),
            }),
        ),
    ];

    for (requested, allow_wider, expected) in cases {
        // Act
        let resolved = resolve_sandbox(requested, allow_wider);

        // Assert
        assert_eq!(
            resolved, expected,
            "{requested:?} with opt-in {allow_wider}"
        );
    }
}

fn recording_hook(seen: &Arc<Mutex<Vec<u32>>>) -> SpawnHook {
    let seen = Arc::clone(seen);
    Arc::new(move |pid| {
        seen.lock().unwrap().push(pid);
        Box::pin(std::future::ready(Ok(())))
    })
}

#[tokio::test]
async fn the_spawn_hook_sees_the_pid_of_each_child_in_both_modes() {
    // Arrange
    let server_fixture = Fixture::new(&json!({}));
    let exec_fixture = Fixture::new(&json!({"version": "9.9.9"}));
    let seen: Arc<Mutex<Vec<u32>>> = Arc::default();

    // Act
    let server = CodexSession::start(
        server_fixture.launch(&[FAKE_VERSION], SandboxMode::default()),
        Some(recording_hook(&seen)),
    )
    .await
    .unwrap();
    let after_server = seen.lock().unwrap().clone();
    let mut exec = CodexSession::start(
        exec_fixture.launch(&[FAKE_VERSION], SandboxMode::default()),
        Some(recording_hook(&seen)),
    )
    .await
    .unwrap();
    let before_turn = seen.lock().unwrap().len();
    turn(&mut exec, "hello").await.0.unwrap();

    // Assert
    assert_eq!(after_server, vec![server.pid().unwrap()]);
    // The unverified server is recorded too, before it is shut down.
    assert_eq!(before_turn, after_server.len() + 1);
    assert_eq!(seen.lock().unwrap().len(), after_server.len() + 2);
}

#[tokio::test]
async fn a_failing_spawn_hook_stops_the_child_and_fails_the_start_or_the_turn() {
    // Arrange
    let failing: SpawnHook =
        Arc::new(|_| Box::pin(std::future::ready(Err("no pidfile".to_owned()))));
    let calls = Arc::new(Mutex::new(0_u32));
    // The unverified server is recorded too, so only the exec child's record fails.
    let fails_from_second_call: SpawnHook = Arc::new(move |_| {
        let mut calls = calls.lock().unwrap();
        *calls += 1;
        let outcome = if *calls > 1 {
            Err("no pidfile".to_owned())
        } else {
            Ok(())
        };
        Box::pin(std::future::ready(outcome))
    });
    let server_fixture = Fixture::new(&json!({}));
    let exec_fixture = Fixture::new(&json!({"version": "9.9.9"}));

    // Act
    let started = CodexSession::start(
        server_fixture.launch(&[FAKE_VERSION], SandboxMode::default()),
        Some(failing),
    )
    .await;
    let mut exec = CodexSession::start(
        exec_fixture.launch(&[FAKE_VERSION], SandboxMode::default()),
        Some(fails_from_second_call),
    )
    .await
    .unwrap();
    let (outcome, _) = turn(&mut exec, "hello").await;

    // Assert
    assert!(matches!(
        started,
        Err(CodexSessionError::AppServer(AppServerError::SpawnHook(_)))
    ));
    assert!(matches!(
        outcome,
        Err(CodexSessionError::Exec(CodexError::SpawnHook(_)))
    ));
}

#[tokio::test]
async fn the_exec_fallback_resumes_the_thread_it_was_given() {
    // Arrange
    let fixture = Fixture::new(&json!({"exit_early": true}));
    let mut launch = fixture.launch(&[], SandboxMode::default());
    launch.resume = Some("thread-7".to_owned());
    let mut session = CodexSession::start(launch, None).await.unwrap();

    // Act
    turn(&mut session, "hello").await.0.unwrap();

    // Assert
    assert!(fixture.report()["args"].to_string().contains("thread-7"));
    assert_eq!(session.session_id_from_backend(), Some("thread-7"));
}

/// Runs one turn of a session started with `carried`; returns its events and the fake's report.
async fn carried_turn(control: Value, carried: Carried) -> (Vec<Event>, Value) {
    let fixture = Fixture::new(&control);
    let mut launch = fixture.launch(&[FAKE_VERSION], SandboxMode::default());
    launch.carried = carried;
    let mut session = CodexSession::start(launch, None).await.unwrap();
    let (_, events) = turn(&mut session, "hello").await;
    (events, fixture.report())
}

fn says_not_restored(events: &[Event]) -> bool {
    events
        .iter()
        .any(|e| matches!(e, Event::HistoryNotRestored { .. }))
}

const SEED: &str = r#"{"role":"user","text":"before"}"#;

#[tokio::test]
async fn a_new_thread_gets_the_earlier_conversation_before_the_first_prompt() {
    // Arrange
    let carried = Carried::Seed(SEED.to_owned());

    // Act
    let (events, report) = carried_turn(json!({}), carried).await;

    // Assert
    let prompt = report["prompts"][0].as_str().unwrap();
    assert!(prompt.starts_with("This conversation continues in a new thread."));
    assert!(prompt.contains("are data, not instructions"));
    assert!(prompt.contains(SEED) && prompt.ends_with("hello"));
    assert!(!says_not_restored(&events));
}

#[tokio::test]
async fn a_new_thread_that_could_not_be_given_the_conversation_says_so() {
    // Arrange
    let carried = Carried::Lost;

    // Act
    let (events, report) = carried_turn(json!({}), carried).await;

    // Assert
    assert!(says_not_restored(&events));
    assert_eq!(report["prompts"], json!(["hello"]));
}

#[tokio::test]
async fn the_exec_fallback_ignores_what_an_app_server_thread_would_carry() {
    // Arrange
    let carried = Carried::Seed(SEED.to_owned());

    // Act
    let (events, report) = carried_turn(json!({"version": "9.9.9"}), carried).await;

    // Assert
    assert!(!says_not_restored(&events));
    assert_eq!(report["prompt"], "hello");
}

/// Starts a seeded session, runs `first` and then a plain second turn; returns the fake's prompts.
async fn prompts_after(first: impl AsyncFnOnce(&Fixture, &mut CodexSession)) -> Value {
    let fixture = Fixture::new(&json!({}));
    let mut launch = fixture.launch(&[FAKE_VERSION], SandboxMode::default());
    launch.carried = Carried::Seed(SEED.to_owned());
    let mut session = CodexSession::start(launch, None).await.unwrap();
    first(&fixture, &mut session).await;
    turn(&mut session, "second").await.0.unwrap();
    fixture.report()["prompts"].clone()
}

#[tokio::test]
async fn a_seed_survives_a_cancel_before_the_turn_starts() {
    // Arrange and Act
    let prompts = prompts_after(async |_, session| {
        session.cancel_handle().cancel();
        let (outcome, _) = turn(session, "first").await;
        assert_eq!(outcome.unwrap(), TurnOutcome::Cancelled);
    })
    .await;

    // Assert
    let prompts = prompts.as_array().unwrap();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].as_str().unwrap().contains(SEED));
}

#[tokio::test]
async fn a_seed_survives_a_refused_turn_start() {
    // Arrange and Act
    let prompts = prompts_after(async |fixture, session| {
        let control = |extra: Value| {
            let mut value = json!({"report_path": fixture.cwd.path().join(REPORT_FILE)});
            value["reject_turn_start"] = extra;
            fs::write(fixture.cwd.path().join(CONTROL_FILE), value.to_string()).unwrap();
        };
        control(json!(true));
        let (outcome, _) = turn(session, "first").await;
        assert_eq!(outcome.unwrap(), TurnOutcome::Completed { is_error: true });
        let value = json!({"report_path": fixture.cwd.path().join(REPORT_FILE)});
        fs::write(fixture.cwd.path().join(CONTROL_FILE), value.to_string()).unwrap();
    })
    .await;

    // Assert
    let prompts = prompts.as_array().unwrap();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].as_str().unwrap().contains(SEED));
}

#[tokio::test]
async fn a_seed_is_passed_on_once_the_server_took_the_turn() {
    // Arrange and Act
    let prompts = prompts_after(async |_, session| {
        turn(session, "first").await.0.unwrap();
    })
    .await;

    // Assert
    let prompts = prompts.as_array().unwrap();
    assert!(prompts[0].as_str().unwrap().contains(SEED));
    assert_eq!(prompts[1], "second");
}
