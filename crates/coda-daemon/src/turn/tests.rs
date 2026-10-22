// SPDX-License-Identifier: AGPL-3.0-only

//! Turns end to end through the dispatcher, against the scripted `fake-claude`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use coda_protocol::events::Event;
use coda_protocol::jsonrpc::Response;
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId, TokenUsage, TurnId};
use coda_store::{NewPending, NewSession, Store, StoredEvent};
use rusqlite::types::ValueRef;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::{BACKEND_UNSUPPORTED, SESSION_NOT_FOUND, TURN_RUNNING, TurnConfig, Turns};
use crate::dispatch::Dispatcher;
use crate::idle_pool::MAX_IDLE_SERVERS;
use crate::state::DaemonState;
use crate::subscribe::{Connection, Hub, Outbox};
use crate::supervisor::testing::RecordingSleeper;
use crate::supervisor::{RestartPolicy, Supervisor};
use crate::trace::BACKEND_SPAN;
use crate::trace::capture::{Spans, install};
use crate::watchdog::Timing;
use coda_core::broker::DECISION_SPAN;

/// An upper bound, not a duration: starting a process can stall for many
/// seconds on a loaded machine, and a wait returns as soon as its event does.
const WAIT: Duration = Duration::from_secs(60);
const POLL: Duration = Duration::from_millis(20);
/// Far above the backend start-up latency, so the idle limit cannot trip before the pause.
const PAUSE_TEST_IDLE_MS: u64 = 5_000;
/// Virtual time to skip: many idle limits, but short of the permission timeout.
const PAUSE_TEST_SKIP: Duration = Duration::from_millis(3 * PAUSE_TEST_IDLE_MS);
const PAUSE_TEST_PERMISSION_MS: u64 = 60_000;
const YIELDS_AFTER_SKIP: usize = 50;
const QUEUE: usize = 256;
const RESTART_BASE: Duration = Duration::from_millis(10);
const STORE_DIR: &str = "store";
const CODEX_REPORT: &str = "codex-report.json";
const PROMPT_CANARY: &str = "CANARY-PROMPT-7f3a91";
const HOME_CANARY: &str = "CANARY-HOME-c05e44";
const ECHO_CANARY: &str = "CANARY-ECHO-19be72";

fn timing(idle_ms: u64, permission_ms: u64) -> Timing {
    Timing {
        idle: Duration::from_millis(idle_ms),
        delegated_tool: Duration::from_secs(30),
        permission: Duration::from_millis(permission_ms),
    }
}

/// Generous idle and permission limits, for tests that are not about timing.
fn relaxed() -> Timing {
    timing(20_000, 20_000)
}

mod restarts;

struct Rig {
    sleeper: Arc<RecordingSleeper>,
    data: TempDir,
    cwd: TempDir,
    hub: Arc<Hub>,
    turns: Arc<Turns>,
    dispatcher: Dispatcher,
    connection: Connection,
    _outbox: Outbox,
    session: SessionId,
    next_id: i64,
}

impl Rig {
    fn new(timing: Timing, parent_env: Vec<(OsString, OsString)>) -> Self {
        Self::build(
            timing,
            parent_env,
            BackendKind::ClaudeCode,
            coda_config::config::Config::default(),
        )
    }

    fn build(
        timing: Timing,
        parent_env: Vec<(OsString, OsString)>,
        backend: BackendKind,
        models: coda_config::config::Config,
    ) -> Self {
        let data = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut store = Store::open(&data.path().join(STORE_DIR)).unwrap();
        let session = store
            .create_session(&NewSession {
                backend,
                model: None,
                cwd: cwd.path().to_string_lossy().into_owned(),
                mode: Mode::Default,
            })
            .unwrap();
        let hub = Hub::new(store, QUEUE);
        let sleeper = Arc::new(RecordingSleeper::default());
        let supervisor = Supervisor::new(
            RestartPolicy::new(RESTART_BASE),
            Arc::clone(&sleeper) as Arc<_>,
        );
        let turns = Turns::with_supervisor(
            Arc::clone(&hub),
            TurnConfig {
                binary: Some(coda_testkit::paths::fake_claude_path().unwrap()),
                pidfile_dir: data.path().to_path_buf(),
                parent_env,
                timing,
                models,
            },
            supervisor,
        );
        let dispatcher = Dispatcher::new()
            .with_hub(Arc::clone(&hub))
            .with_state(DaemonState::new(data.path().to_path_buf()))
            .with_turns(Arc::clone(&turns));
        let (connection, outbox) = dispatcher.connect();
        Self {
            sleeper,
            data,
            cwd,
            hub,
            turns,
            dispatcher,
            connection,
            _outbox: outbox,
            session,
            next_id: 0,
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        self.request(self.next_id, method, params).await
    }

    /// Like `call` with a caller-chosen id, so two requests can be in flight at once.
    async fn request(&self, id: i64, method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let payload = serde_json::to_vec(&request).unwrap();
        let response: Response = self
            .dispatcher
            .handle(&payload, &self.connection)
            .await
            .unwrap();
        serde_json::to_value(response).unwrap()
    }

    async fn send(&mut self, text: &str) -> Value {
        let params = json!({"session_id": self.session, "text": text});
        self.call("session.send", params).await
    }

    async fn events(&self) -> Vec<StoredEvent> {
        let session = self.session.clone();
        self.hub
            .with_store(move |store| store.load_events(&session, 0, 1000))
            .await
            .unwrap()
    }

    async fn kinds(&self) -> Vec<String> {
        self.events().await.into_iter().map(|e| e.kind).collect()
    }

    /// Polls until an event of `kind` is stored, then returns it.
    async fn wait_for(&self, kind: &str) -> StoredEvent {
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(event) = self.events().await.into_iter().find(|e| e.kind == kind) {
                    return event;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {kind} event was stored in time"))
    }

    async fn pending(&self) -> Vec<coda_store::PendingPermission> {
        let session = self.session.clone();
        self.hub
            .with_store(move |store| store.list_pending(&session))
            .await
            .unwrap()
    }

    /// Polls until a permission request is waiting and returns its id.
    async fn wait_for_pending(&self) -> String {
        tokio::time::timeout(WAIT, async {
            loop {
                if let Some(first) = self.pending().await.first() {
                    return first.id.as_str().to_owned();
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("no permission request was queued in time")
    }

    async fn respond(&mut self, request_id: &str, decision: &str) -> Value {
        let session = self.session.clone();
        self.respond_as(&session, request_id, decision).await
    }

    async fn respond_as(&mut self, session: &SessionId, request_id: &str, decision: &str) -> Value {
        let params = json!({
            "session_id": session, "request_id": request_id, "decision": decision,
        });
        self.call("permission.respond", params).await
    }

    async fn other_session(&self) -> SessionId {
        let cwd = self.cwd.path().to_string_lossy().into_owned();
        self.hub
            .with_store_mut(move |store| {
                store.create_session(&NewSession {
                    backend: BackendKind::ClaudeCode,
                    model: None,
                    cwd,
                    mode: Mode::Default,
                })
            })
            .await
            .unwrap()
    }

    fn db(&self) -> rusqlite::Connection {
        let path = self.data.path().join(STORE_DIR);
        rusqlite::Connection::open(path.join(coda_store::DB_FILE_NAME)).unwrap()
    }

    /// Tells `fake-claude` what to do, through the file it reads from its cwd.
    fn control(&self, value: &Value) {
        let file = self.cwd.path().join("fake-claude-control.json");
        std::fs::write(file, value.to_string()).unwrap();
    }

    fn pidfile(&self) -> PathBuf {
        crate::backends::pidfile_path(self.data.path(), &self.session)
    }

    async fn wait_until_idle(&self) {
        let path = self.pidfile();
        tokio::time::timeout(WAIT, async {
            while path.exists() || self.turns.is_running(&self.session) {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the turn did not clean up in time");
    }
}

#[tokio::test]
async fn a_sent_message_runs_a_turn_that_ends_with_a_result() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let sent = rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    let answered = rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;

    // Assert
    assert!(sent["result"]["turn_id"].is_string());
    assert_eq!(answered["result"]["accepted"], true);
    let kinds = rig.kinds().await;
    for expected in [
        "user_message",
        "stream_delta",
        "permission_request",
        "approve",
        "result",
    ] {
        assert!(
            kinds.iter().any(|k| k == expected),
            "missing {expected} in {kinds:?}"
        );
    }
}

#[tokio::test]
async fn a_denied_request_is_stored_as_a_deny_event() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;

    // Act
    let answered = rig.respond(&request, "deny").await;
    let event = rig.wait_for("deny").await;

    // Assert
    assert_eq!(answered["result"]["accepted"], true);
    let body: Value = serde_json::from_str(&event.body).unwrap();
    assert_eq!(body["tool"], "bash");
    assert!(!rig.kinds().await.contains(&"approve".to_owned()));
}

#[tokio::test]
async fn the_pidfile_exists_while_the_turn_runs_and_is_gone_after_it() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    let during = rig.pidfile().exists();
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(during);
    assert!(!rig.pidfile().exists());
}

#[tokio::test]
async fn a_second_send_while_a_turn_runs_is_refused() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let first = rig.send("hello").await;

    // Act
    let second = rig.send("again").await;

    // Assert
    assert!(first.get("error").is_none(), "{first}");
    assert_eq!(second["error"]["code"], TURN_RUNNING);
}

#[tokio::test]
async fn answering_an_unknown_request_is_not_accepted() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let answered = rig.respond("999", "allow_once").await;

    // Assert
    assert_eq!(answered["result"]["accepted"], false);
}

#[tokio::test]
async fn daemon_status_counts_the_running_turn() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;

    // Act
    let during = rig.call("daemon.status", json!({})).await;
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;
    let after = rig.call("daemon.status", json!({})).await;

    // Assert
    assert_eq!(during["result"]["active_turns"], 1);
    assert_eq!(after["result"]["active_turns"], 0);
}

#[tokio::test]
async fn cancelling_stops_the_turn_and_settles_the_waiting_prompt() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    rig.wait_for_pending().await;

    // Act
    let cancelled = rig
        .call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_for("turn_interrupted").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(cancelled["result"]["cancelled"], true);
    assert_eq!(rig.pending().await.len(), 0);
}

#[tokio::test]
async fn cancelling_a_session_without_a_turn_reports_nothing_cancelled() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let cancelled = rig
        .call("session.cancel", json!({"session_id": rig.session}))
        .await;

    // Assert
    assert_eq!(cancelled["result"]["cancelled"], false);
}

#[tokio::test]
async fn a_cancel_while_the_backend_is_starting_ends_the_turn_cancelled() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    let gate = rig.cwd.path().join("start.gate");
    rig.control(&json!({ "start_gate": gate }));
    let params = json!({"session_id": rig.session, "text": "hello"});

    // Act
    let held_send = rig.request(1, "session.send", params);
    let cancel_then_release = async {
        tokio::time::timeout(WAIT, async {
            while !rig.turns.is_running(&rig.session) {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the send never reserved the session");
        let cancelled = rig
            .request(2, "session.cancel", json!({"session_id": rig.session}))
            .await;
        std::fs::write(&gate, "").unwrap();
        cancelled
    };
    let (_, cancelled) = tokio::join!(held_send, cancel_then_release);

    // Assert
    assert_eq!(cancelled["result"]["cancelled"], true);
    rig.wait_for("turn_interrupted").await;
    rig.wait_until_idle().await;
    assert_eq!(rig.pending().await.len(), 0);
    let kinds = rig.kinds().await;
    assert!(
        !kinds.iter().any(|k| k == "permission_request"),
        "{kinds:?}"
    );
}

#[tokio::test]
async fn a_cancel_with_no_turn_does_not_cancel_the_next_turn() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let cancelled = rig
        .call("session.cancel", json!({"session_id": rig.session}))
        .await;

    // Act
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;

    // Assert
    assert_eq!(cancelled["result"]["cancelled"], false);
    assert!(!rig.kinds().await.iter().any(|k| k == "turn_interrupted"));
}

#[tokio::test]
async fn a_crashed_backend_is_stored_as_a_crash_event_with_its_exit_code() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    rig.send("please crash").await;
    let crash = rig.wait_for("backend_crashed").await;

    // Assert
    let body: Value = serde_json::from_str(&crash.body).unwrap();
    assert_eq!(body["exit_code"], 3);
}

#[tokio::test]
async fn a_silent_backend_is_aborted_by_the_idle_timeout() {
    // Arrange
    let mut rig = Rig::new(timing(150, 20_000), Vec::new());

    // Act
    rig.send("please stall").await;
    rig.wait_for("turn_interrupted").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(!rig.kinds().await.contains(&"result".to_owned()));
}

/// Short enough that a slow tool outlasts it several times over.
const TOOL_TEST_IDLE_MS: u64 = 500;
/// Several times the idle limit, so reaching it shows the tool bound applied, not the idle one.
const TOOL_TEST_BOUND_MS: u64 = 4 * TOOL_TEST_IDLE_MS;
const TOOL_TEST_LONG_BOUND_MS: u64 = 60_000;
/// Long enough to tell from the idle limit, short enough that a stuck run fails fast.
const TOOL_TEST_RESTART_BOUND_MS: u64 = 20_000;

/// An idle limit below the fakes' silent tool, and a running-tool bound above it
/// or, for a hung tool, below the wait a test can afford.
fn tool_timing(tool_bound_ms: u64) -> Timing {
    Timing {
        idle: Duration::from_millis(TOOL_TEST_IDLE_MS),
        delegated_tool: Duration::from_millis(tool_bound_ms),
        permission: Duration::from_secs(20),
    }
}

#[tokio::test]
async fn a_claude_tool_running_silently_past_the_idle_limit_does_not_abort_the_turn() {
    // Arrange
    let mut rig = Rig::new(tool_timing(TOOL_TEST_LONG_BOUND_MS), Vec::new());

    // Act
    rig.send("tool-slow").await;
    rig.wait_until_idle().await;

    // Assert
    let kinds = rig.kinds().await;
    assert!(kinds.contains(&"result".to_owned()), "{kinds:?}");
    assert!(!kinds.contains(&"turn_interrupted".to_owned()));
}

#[tokio::test]
async fn a_claude_tool_that_never_reports_ends_the_turn_at_the_tool_bound() {
    // Arrange
    let mut rig = Rig::new(tool_timing(TOOL_TEST_BOUND_MS), Vec::new());

    // Act
    let started = Instant::now();
    rig.send("tool-hang").await;
    rig.wait_for("turn_interrupted").await;
    let waited = started.elapsed();
    rig.wait_until_idle().await;

    // Assert
    assert!(
        waited >= Duration::from_millis(TOOL_TEST_BOUND_MS),
        "{waited:?}"
    );
    assert!(!rig.kinds().await.contains(&"result".to_owned()));
}

#[tokio::test]
async fn a_codex_command_running_silently_past_the_idle_limit_does_not_abort_the_turn() {
    // Arrange
    let mut rig = codex_rig_with(tool_timing(TOOL_TEST_LONG_BOUND_MS), &json!({}));

    // Act
    rig.send("tool-slow").await;
    rig.wait_until_turn_over().await;

    // Assert
    let kinds = rig.kinds().await;
    assert!(kinds.contains(&"result".to_owned()), "{kinds:?}");
    assert!(!kinds.contains(&"turn_interrupted".to_owned()));
}

#[tokio::test]
async fn a_codex_command_that_never_completes_ends_the_turn_at_the_tool_bound() {
    // Arrange
    let mut rig = codex_rig_with(tool_timing(TOOL_TEST_BOUND_MS), &json!({}));

    // Act
    let started = Instant::now();
    rig.send("tool-hang").await;
    rig.wait_for("turn_interrupted").await;
    let waited = started.elapsed();
    rig.wait_until_turn_over().await;

    // Assert
    assert!(
        waited >= Duration::from_millis(TOOL_TEST_BOUND_MS),
        "{waited:?}"
    );
    assert!(!rig.kinds().await.contains(&"result".to_owned()));
}

#[tokio::test]
async fn claude_streaming_tool_input_and_thinking_past_the_idle_limit_is_not_stuck() {
    // Arrange
    let mut rig = Rig::new(tool_timing(TOOL_TEST_LONG_BOUND_MS), Vec::new());

    // Act
    rig.send("stream-long").await;
    rig.wait_until_idle().await;

    // Assert
    let kinds = rig.kinds().await;
    assert!(kinds.contains(&"result".to_owned()), "{kinds:?}");
    assert!(!kinds.contains(&"turn_interrupted".to_owned()));
}

#[tokio::test]
async fn codex_streaming_reasoning_past_the_idle_limit_is_not_stuck() {
    // Arrange
    let mut rig = codex_rig_with(tool_timing(TOOL_TEST_LONG_BOUND_MS), &json!({}));

    // Act
    rig.send("stream-long").await;
    rig.wait_until_turn_over().await;

    // Assert
    let kinds = rig.kinds().await;
    assert!(kinds.contains(&"result".to_owned()), "{kinds:?}");
    assert!(!kinds.contains(&"turn_interrupted".to_owned()));
}

#[tokio::test]
async fn a_restart_drops_the_tools_the_crashed_backend_left_in_flight() {
    // Arrange
    let mut rig = Rig::new(tool_timing(TOOL_TEST_RESTART_BOUND_MS), Vec::new());
    rig.control(&json!({"crash_marker": rig.cwd.path().join("crashed")}));

    // Act
    let started = Instant::now();
    rig.send("tool-flaky").await;
    rig.wait_for("turn_interrupted").await;
    let waited = started.elapsed();
    rig.wait_until_idle().await;

    // Assert
    assert!(
        waited < Duration::from_millis(TOOL_TEST_RESTART_BOUND_MS / 2),
        "{waited:?}"
    );
}

#[tokio::test]
async fn a_pending_permission_pauses_the_idle_timeout() {
    // Arrange
    let mut rig = Rig::new(
        timing(PAUSE_TEST_IDLE_MS, PAUSE_TEST_PERMISSION_MS),
        Vec::new(),
    );
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;

    // Act
    // Freeze the clock only now: skipping virtual time trips the idle limit
    // if the pending permission did not pause the watchdog.
    tokio::time::pause();
    tokio::time::advance(PAUSE_TEST_SKIP).await;
    for _ in 0..YIELDS_AFTER_SKIP {
        tokio::task::yield_now().await;
    }
    let interrupted_while_waiting = rig.kinds().await.contains(&"turn_interrupted".to_owned());
    tokio::time::resume();
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;

    // Assert
    assert!(!interrupted_while_waiting);
}

#[tokio::test]
async fn an_unanswered_permission_is_denied_after_its_timeout_without_a_client() {
    // Arrange
    let mut rig = Rig::new(timing(20_000, 100), Vec::new());

    // Act
    rig.send("hello").await;
    rig.wait_for("result").await;

    // Assert
    assert!(rig.kinds().await.contains(&"deny".to_owned()));
}

#[tokio::test]
async fn sending_to_an_unknown_session_is_a_session_not_found_error() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let result = rig
        .call("session.send", json!({"session_id": "999", "text": "hi"}))
        .await;

    // Assert
    assert_eq!(result["error"]["code"], SESSION_NOT_FOUND);
}

#[tokio::test]
async fn an_answer_from_another_session_is_refused_and_changes_nothing() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    let intruder = rig.other_session().await;

    // Act
    let refused = rig.respond_as(&intruder, &request, "allow_once").await;

    // Assert
    assert_eq!(refused["result"]["accepted"], false);
    assert_eq!(rig.pending().await.len(), 1);
    assert!(!rig.kinds().await.contains(&"approve".to_owned()));
    // The owner's answer still works, and the turn's end proves the refused
    // answer left nothing behind: exactly one approval was stored.
    let accepted = rig.respond(&request, "allow_once").await;
    assert_eq!(accepted["result"]["accepted"], true);
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
    let approvals = rig.kinds().await.iter().filter(|k| *k == "approve").count();
    assert_eq!(approvals, 1);
}

#[test]
fn leftover_prompts_are_denied_even_when_a_client_answered_one_after_the_listing() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(&dir.path().join(STORE_DIR)).unwrap();
    let new = NewSession {
        backend: BackendKind::ClaudeCode,
        model: None,
        cwd: "/ws".to_owned(),
        mode: Mode::Default,
    };
    let session = store.create_session(&new).unwrap();
    for command in ["ls", "pwd"] {
        let pending = NewPending {
            session: session.clone(),
            tool: "bash".to_owned(),
            repo_path: "/ws".to_owned(),
            command: command.to_owned(),
        };
        store.add_pending(&pending).unwrap();
    }
    let listed = store.list_pending(&session).unwrap();
    store
        .resolve_pending(&listed[0].id, PermissionDecision::AllowOnce)
        .unwrap();

    // Act
    super::deny_unanswered(&mut store, &listed);

    // Assert
    assert_eq!(store.list_pending(&session).unwrap().len(), 0);
}

#[tokio::test]
async fn a_failure_while_recording_the_turn_start_removes_the_pidfile() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.db().execute_batch("DROP TABLE turns").unwrap();

    // Act
    let sent = rig.send("hello").await;

    // Assert
    assert_eq!(
        sent["error"]["code"],
        coda_protocol::jsonrpc::INTERNAL_ERROR
    );
    assert!(!rig.pidfile().exists());
}

#[tokio::test]
async fn a_panicking_turn_task_does_not_leave_the_session_busy() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let slot = rig.turns.reserve(&rig.session).unwrap();
    let busy = rig.send("hello").await;

    // Act
    let task = super::spawn_guarded(slot, async { panic!("simulated turn failure") });
    let joined = task.await;
    let again = rig.send("hello").await;

    // Assert
    assert_eq!(busy["error"]["code"], TURN_RUNNING);
    assert!(joined.is_err());
    assert!(again["result"]["turn_id"].is_string());
}

#[tokio::test]
async fn a_cancelled_turn_is_logged_as_ended_and_not_as_an_error() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    rig.wait_for_pending().await;

    // Act
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_for("turn_interrupted").await;
    rig.wait_until_idle().await;

    // Assert
    let log = dump(&rig.db(), "event_log");
    assert!(log.contains("turn_ended"), "positive control: {log}");
    assert!(!log.contains("error"), "{log}");
}

#[tokio::test]
async fn a_turn_is_ended_even_when_the_event_log_refuses_its_end_record() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    rig.wait_for_pending().await;
    rig.db()
        .execute_batch(
            "CREATE TRIGGER refuse_log BEFORE INSERT ON event_log \
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();

    // Act
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_until_idle().await;

    // Assert
    let open: i64 = rig
        .db()
        .query_row(
            "SELECT COUNT(*) FROM turns WHERE ended_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(open, 0);
}

#[tokio::test]
async fn a_turn_whose_first_event_cannot_be_stored_is_not_left_open() {
    // Arrange
    let rig = Rig::new(relaxed(), Vec::new());
    rig.db()
        .execute_batch(
            "CREATE TRIGGER refuse_events BEFORE INSERT ON events \
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();

    // Act
    let begun = Turns::begin(&rig.hub, &rig.session, "hello").await;

    // Assert
    let open = rig.hub.with_store(Store::unfinished_turns).await.unwrap();
    assert!(begun.is_err());
    assert_eq!(open, vec![]);
}

#[tokio::test]
async fn methods_are_unavailable_without_turns() {
    // Arrange
    let dispatcher = Dispatcher::new();
    let (connection, _outbox) = dispatcher.connect();
    let payload =
        br#"{"jsonrpc":"2.0","id":1,"method":"session.cancel","params":{"session_id":"1"}}"#;

    // Act
    let response = dispatcher.handle(payload, &connection).await.unwrap();

    // Assert
    let value = serde_json::to_value(response).unwrap();
    assert_eq!(value["error"]["code"], -32601);
    assert_ne!(BACKEND_UNSUPPORTED, TURN_RUNNING);
}

#[tokio::test]
async fn the_backend_process_and_each_permission_decision_have_spans() {
    // Arrange
    let (spans, _guard) = install();
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;

    // Act
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;

    // Assert
    let backend = spans.named(BACKEND_SPAN);
    let permission = spans.named(DECISION_SPAN);
    assert_eq!(backend.len(), 1);
    assert_eq!(permission.len(), 1);
    assert_eq!(permission[0].fields["coda.tool.name"], "bash");
    assert_eq!(backend[0].fields["coda.session.id"], rig.session.as_str());
}

/// Every cell of every row of `table`, as one string.
fn dump(db: &rusqlite::Connection, table: &str) -> String {
    let mut stmt = db.prepare(&format!("SELECT * FROM {table}")).unwrap();
    let columns = stmt.column_count();
    let mut rows = stmt.query([]).unwrap();
    let mut out = String::new();
    while let Some(row) = rows.next().unwrap() {
        for index in 0..columns {
            let cell = match row.get_ref(index).unwrap() {
                ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
                    String::from_utf8_lossy(bytes).into_owned()
                }
                other => format!("{other:?}"),
            };
            out.push_str(&cell);
            out.push('|');
        }
        out.push('\n');
    }
    out
}

fn tables(db: &rusqlite::Connection) -> Vec<String> {
    let mut stmt = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn span_text(spans: &Spans) -> String {
    format!("{:?}", spans.all())
}

#[tokio::test]
async fn a_prompt_canary_is_stored_as_content_and_never_in_logs_usage_or_spans() {
    // Arrange
    let (spans, _guard) = install();
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let sent = rig.send(&format!("hello {PROMPT_CANARY}")).await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
    let turn = TurnId::new(sent["result"]["turn_id"].as_str().unwrap());
    let usage = Event::Usage {
        session_id: rig.session.clone(),
        turn_id: turn.clone(),
        usage: TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    };
    super::store_event(&rig.hub, &rig.session, &turn, &usage)
        .await
        .unwrap();

    // Assert
    let db = rig.db();
    assert!(
        dump(&db, "events").contains(PROMPT_CANARY),
        "positive control"
    );
    assert!(!dump(&db, "usage").is_empty(), "usage positive control");
    for table in ["event_log", "usage"] {
        assert!(
            !dump(&db, table).contains(PROMPT_CANARY),
            "{table} holds the prompt"
        );
    }
    assert!(!dump(&db, "turns").contains(PROMPT_CANARY));
    assert!(!span_text(&spans).contains(PROMPT_CANARY));
    assert!(spans.all().len() > 1);
}

#[tokio::test]
async fn a_home_directory_name_the_backend_receives_is_never_written_by_the_daemon() {
    // Arrange
    let (spans, _guard) = install();
    let home: OsString = format!("/nonexistent/{HOME_CANARY}").into();
    let mut rig = Rig::new(relaxed(), vec![("HOME".into(), home)]);
    let report = rig.cwd.path().join("report.json");
    rig.control(&json!({ "report_path": report }));

    // Act
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let seen: Value = serde_json::from_str(&std::fs::read_to_string(report).unwrap()).unwrap();
    assert!(
        seen["home"].as_str().unwrap().contains(HOME_CANARY),
        "positive control: the backend was given the HOME canary"
    );
    let db = rig.db();
    let all = tables(&db);
    assert!(all.iter().any(|t| t == "event_log") && all.iter().any(|t| t == "usage"));
    for table in &all {
        assert!(
            !dump(&db, table).contains(HOME_CANARY),
            "{table} holds the HOME name"
        );
    }
    assert!(!span_text(&spans).contains(HOME_CANARY));
}

#[tokio::test]
async fn text_the_backend_emits_is_stored_as_content() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.control(&json!({ "echo_text": ECHO_CANARY }));

    // Act
    rig.send("please echo-text").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;

    // Assert
    assert!(dump(&rig.db(), "events").contains(ECHO_CANARY));
}

fn codex_config() -> coda_config::config::Config {
    let binary = coda_testkit::paths::fake_codex_path().unwrap();
    let toml = format!(
        "[providers.codex]\nenabled = true\nbinary = \"{}\"\nverified_versions = [\"0.0.0-fake\"]\n",
        binary.display()
    );
    coda_config::config::Config::from_toml_str(&toml)
        .unwrap()
        .config
}

fn codex_rig(control: &Value) -> Rig {
    codex_rig_with(relaxed(), control)
}

fn codex_rig_with(timing: Timing, control: &Value) -> Rig {
    let rig = Rig::build(
        timing,
        Vec::new(),
        BackendKind::CodexAppServer,
        codex_config(),
    );
    rig.write_codex_control(control);
    rig
}

impl Rig {
    fn write_codex_control(&self, control: &Value) {
        let mut control = control.clone();
        control["report_path"] = json!(self.cwd.path().join(CODEX_REPORT));
        let file = self.cwd.path().join("fake-codex-control.json");
        std::fs::write(file, control.to_string()).unwrap();
    }

    async fn codex_session(&self) -> SessionId {
        let cwd = self.cwd.path().to_string_lossy().into_owned();
        self.hub
            .with_store_mut(move |store| {
                store.create_session(&NewSession {
                    backend: BackendKind::CodexAppServer,
                    model: None,
                    cwd,
                    mode: Mode::Default,
                })
            })
            .await
            .unwrap()
    }

    /// Sends `text` to `session` and waits until its turn is over.
    async fn run_turn_of(&mut self, session: &SessionId, text: &str) {
        self.call("session.send", json!({"session_id": session, "text": text}))
            .await;
        tokio::time::timeout(WAIT, async {
            while self.turns.is_running(session) {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the turn did not end in time");
    }

    /// The fake app-server's report of the server that is running now.
    fn codex_report(&self) -> Value {
        let raw = std::fs::read_to_string(self.cwd.path().join(CODEX_REPORT)).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn codex_pid(&self) -> rustix::process::Pid {
        let pid = self.codex_report()["pid"].as_i64().unwrap();
        rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap()
    }

    /// Polls until the turn has ended and its slot is free; a pooled server keeps its pidfile.
    async fn wait_until_turn_over(&self) {
        tokio::time::timeout(WAIT, async {
            while self.turns.is_running(&self.session) {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the turn did not end in time");
    }

    /// Polls until the fake has received `count` prompts since it started.
    async fn wait_for_prompts(&self, count: usize) {
        tokio::time::timeout(WAIT, async {
            loop {
                let seen = std::fs::read_to_string(self.cwd.path().join(CODEX_REPORT))
                    .ok()
                    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                    .and_then(|report| report["prompts"].as_array().map(Vec::len));
                if seen >= Some(count) {
                    return;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the server did not receive the prompt in time");
    }
}

async fn assert_gone(pid: rustix::process::Pid) {
    tokio::time::timeout(WAIT, async {
        while rustix::process::test_kill_process(pid).is_ok() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the server process is still alive");
}

#[tokio::test]
async fn two_turns_of_a_codex_session_run_on_one_server_without_a_not_carried_notice() {
    // Arrange
    let mut rig = codex_rig(&json!({}));

    // Act
    let sent = rig.send("hello").await;
    rig.wait_until_turn_over().await;
    let pid = rig.codex_pid();
    rig.send("second").await;
    rig.wait_until_turn_over().await;

    // Assert
    assert!(sent["result"]["turn_id"].is_string());
    let kinds = rig.kinds().await;
    assert!(kinds.contains(&"stream_delta".to_owned()));
    assert!(!kinds.contains(&"approvals_unavailable".to_owned()));
    assert_eq!(rig.codex_pid(), pid);
    assert_eq!(rig.codex_report()["prompts"], json!(["hello", "second"]));
    assert!(rig.pidfile().exists());
    assert!(!dump(&rig.db(), "events").contains("do not carry over"));
}

#[tokio::test]
async fn a_server_that_dies_mid_turn_ends_that_turn_and_the_next_respawns_it_with_the_history() {
    // Arrange
    let mut rig = codex_rig(&json!({}));
    rig.send("hello").await;
    rig.wait_until_turn_over().await;
    let first = rig.codex_pid();

    // Act
    rig.send("please crash").await;
    rig.wait_until_turn_over().await;
    let after_crash = rig.codex_pid();
    rig.send("again").await;
    rig.wait_until_turn_over().await;

    // Assert
    assert_eq!(after_crash, first);
    let crashes = rig
        .events()
        .await
        .into_iter()
        .filter(|e| e.kind == "backend_crashed")
        .count();
    assert_eq!(crashes, 1);
    let crash = rig
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == "backend_crashed")
        .unwrap();
    let body: Value = serde_json::from_str(&crash.body).unwrap();
    assert_eq!(
        body["detail"],
        "the Codex app-server exited during the turn; the next turn starts a new server"
    );
    assert_ne!(rig.codex_pid(), first);
    let prompts = rig.codex_report()["prompts"].to_string();
    assert!(prompts.contains("hello") && prompts.contains("working on it"));
    assert!(prompts.contains("again"));
}

#[tokio::test]
async fn a_second_send_is_rejected_with_turn_running_and_a_cancel_leaves_the_server_usable() {
    // Arrange
    let mut rig = codex_rig(&json!({}));
    rig.send("please stall").await;
    rig.wait_for_prompts(1).await;
    let pid = rig.codex_pid();

    // Act
    let busy = rig.send("meanwhile").await;
    let cancelled = rig
        .call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_until_turn_over().await;
    rig.send("after").await;
    rig.wait_until_turn_over().await;

    // Assert
    assert_eq!(busy["error"]["code"], TURN_RUNNING);
    assert_eq!(cancelled["result"]["cancelled"], true);
    assert_eq!(rig.codex_pid(), pid);
    assert!(rig.codex_report()["interrupt_params"]["turnId"].is_string());
    assert!(rig.kinds().await.contains(&"turn_interrupted".to_owned()));
}

#[tokio::test]
async fn shutting_down_stops_the_pooled_codex_server() {
    // Arrange
    let mut rig = codex_rig(&json!({}));
    rig.send("hello").await;
    rig.wait_until_turn_over().await;
    let pid = rig.codex_pid();
    let idle_alive = rustix::process::test_kill_process(pid).is_ok();

    // Act
    rig.turns.shutdown().await;

    // Assert
    assert!(idle_alive);
    assert_gone(pid).await;
    assert!(!rig.pidfile().exists());
}

#[tokio::test]
async fn a_codex_turn_on_the_exec_fallback_stores_the_approvals_unavailable_event() {
    // Arrange
    let mut rig = codex_rig(&json!({"version": "9.9.9"}));

    // Act
    rig.send("hello").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let kinds = rig.kinds().await;
    assert!(kinds.contains(&"approvals_unavailable".to_owned()));
    assert!(!rig.pidfile().exists());
}

#[tokio::test]
async fn a_codex_app_server_crash_is_stored_as_a_crash_event_without_a_fallback() {
    // Arrange
    let mut rig = codex_rig(&json!({}));

    // Act
    rig.send("please crash").await;
    rig.wait_for("backend_crashed").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(
        !rig.kinds()
            .await
            .contains(&"approvals_unavailable".to_owned())
    );
    let log = dump(&rig.db(), "event_log");
    assert!(log.contains("turn_ended") && log.contains("error"), "{log}");
}

#[tokio::test]
async fn a_respawn_that_fails_ends_the_send_with_an_error_and_is_not_retried() {
    // Arrange
    let mut rig = codex_rig(&json!({}));
    rig.send("hello").await;
    rig.wait_until_turn_over().await;
    let first = rig.codex_pid();
    rig.send("please crash").await;
    rig.wait_until_turn_over().await;
    rig.write_codex_control(&json!({"exit_early": true}));

    // Act
    let refused = rig.send("again").await;

    // Assert
    assert!(refused["error"].is_object(), "{refused}");
    assert!(!rig.turns.is_running(&rig.session));
    rig.write_codex_control(&json!({}));
    rig.send("third").await;
    rig.wait_until_turn_over().await;
    assert_ne!(rig.codex_pid(), first);
    assert!(rustix::process::test_kill_process(rig.codex_pid()).is_ok());
}

#[tokio::test]
async fn the_least_recently_used_idle_server_is_stopped_once_the_pool_is_full() {
    // Arrange
    let mut rig = codex_rig(&json!({}));
    let first = rig.session.clone();
    rig.run_turn_of(&first, "one").await;
    let first_pid = rig.codex_pid();
    let mut later = Vec::new();

    // Act
    for _ in 0..MAX_IDLE_SERVERS {
        let session = rig.codex_session().await;
        rig.run_turn_of(&session, "filler").await;
        later.push((session.clone(), rig.codex_pid()));
    }

    // Assert
    assert_gone(first_pid).await;
    assert!(!crate::backends::pidfile_path(rig.data.path(), &first).exists());
    for (session, pid) in &later {
        assert!(rustix::process::test_kill_process(*pid).is_ok());
        assert!(crate::backends::pidfile_path(rig.data.path(), session).exists());
    }
}

#[test]
fn a_pidfile_naming_another_server_is_left_alone() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let session = SessionId::new("s-1");
    let pid = std::process::id();
    let path = crate::backends::pidfile_path(dir.path(), &session);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(crate::pidfile::write(
            &path,
            pid,
            std::path::Path::new("/bin/sh"),
        ))
        .unwrap();

    // Act
    super::remove_pidfile_of(dir.path(), &session, Some(pid + 1));
    let kept = path.exists();
    super::remove_pidfile_of(dir.path(), &session, Some(pid));

    // Assert
    assert!(kept);
    assert!(!path.exists());
}
