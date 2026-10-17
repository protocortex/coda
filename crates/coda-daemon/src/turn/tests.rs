// SPDX-License-Identifier: AGPL-3.0-only

//! Turns end to end through the dispatcher, against the scripted `fake-claude`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use coda_protocol::events::Event;
use coda_protocol::jsonrpc::Response;
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId, TokenUsage, TurnId};
use coda_store::{NewPending, NewSession, Store, StoredEvent};
use rusqlite::types::ValueRef;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::{BACKEND_UNSUPPORTED, SESSION_NOT_FOUND, TURN_RUNNING, TurnConfig, Turns};
use crate::dispatch::Dispatcher;
use crate::state::DaemonState;
use crate::subscribe::{Connection, Hub, Outbox};
use crate::trace::capture::{Spans, install};
use crate::trace::{BACKEND_SPAN, PERMISSION_SPAN};
use crate::watchdog::Timing;

const WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(20);
const QUIET_PERIOD: Duration = Duration::from_millis(400);
/// Far above the backend start-up latency, so the idle limit cannot trip before the pause.
const PAUSE_TEST_IDLE_MS: u64 = 5_000;
/// Virtual time to skip: many idle limits, but short of the permission timeout.
const PAUSE_TEST_SKIP: Duration = Duration::from_millis(3 * PAUSE_TEST_IDLE_MS);
const PAUSE_TEST_PERMISSION_MS: u64 = 60_000;
const YIELDS_AFTER_SKIP: usize = 50;
const QUEUE: usize = 256;
const STORE_DIR: &str = "store";
const PROMPT_CANARY: &str = "CANARY-PROMPT-7f3a91";
const HOME_CANARY: &str = "CANARY-HOME-c05e44";
const ECHO_CANARY: &str = "CANARY-ECHO-19be72";

fn timing(idle_ms: u64, permission_ms: u64) -> Timing {
    Timing {
        idle: Duration::from_millis(idle_ms),
        tool_execution: Duration::from_secs(30),
        permission: Duration::from_millis(permission_ms),
    }
}

/// Generous idle and permission limits, for tests that are not about timing.
fn relaxed() -> Timing {
    timing(20_000, 20_000)
}

struct Rig {
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
        let data = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut store = Store::open(&data.path().join(STORE_DIR)).unwrap();
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::ClaudeCode,
                model: None,
                cwd: cwd.path().to_string_lossy().into_owned(),
                mode: Mode::Default,
            })
            .unwrap();
        let hub = Hub::new(store, QUEUE);
        let turns = Turns::new(
            Arc::clone(&hub),
            TurnConfig {
                binary: coda_testkit::paths::fake_claude_path().unwrap(),
                pidfile_dir: data.path().to_path_buf(),
                parent_env,
                timing,
            },
        );
        let dispatcher = Dispatcher::new()
            .with_hub(Arc::clone(&hub))
            .with_state(DaemonState::new(data.path().to_path_buf()))
            .with_turns(Arc::clone(&turns));
        let (connection, outbox) = dispatcher.connect();
        Self {
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
        let request =
            json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params});
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
    tokio::time::sleep(QUIET_PERIOD).await;

    // Assert
    assert_eq!(refused["result"]["accepted"], false);
    assert_eq!(rig.pending().await.len(), 1);
    assert!(!rig.kinds().await.contains(&"approve".to_owned()));
    let accepted = rig.respond(&request, "allow_once").await;
    assert_eq!(accepted["result"]["accepted"], true);
    rig.wait_for("result").await;
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
    tokio::time::sleep(QUIET_PERIOD).await;

    // Assert
    let log = dump(&rig.db(), "event_log");
    assert!(log.contains("turn_ended"), "positive control: {log}");
    assert!(!log.contains("error"), "{log}");
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
    let permission = spans.named(PERMISSION_SPAN);
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
