// SPDX-License-Identifier: AGPL-3.0-only

//! Model-API sessions end to end through the dispatcher, against the faux provider.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use coda_config::config::{Config, ProviderConfig};
use coda_protocol::jsonrpc::Response;
use coda_protocol::types::{BackendKind, Mode, SessionId, TurnId};
use coda_providers::model::{ModelProvider, ModelRequest};
use coda_store::{NewSession, SessionRecord, Store, StoredEvent};
use rusqlite::types::ValueRef;
use serde_json::{Value, json};
use tempfile::TempDir;

use super::{CONVERSATION_RESET_KIND, EnvReader, ModelSessions, keep_recent, lock};
use crate::dispatch::Dispatcher;
use crate::gate::Brokers;
use crate::state::DaemonState;
use crate::subscribe::{Connection, Hub, Outbox};
use crate::trace::capture::{Spans, install};
use crate::turn::{BACKEND_UNSUPPORTED, TurnConfig, Turns};
use crate::watchdog::Timing;
use coda_core::turn::{CancelToken, PROVIDER_CALL_SPAN, TOOL_CALL_SPAN};

const WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(20);
const QUEUE: usize = 256;
const PROMPT_CANARY: &str = "CANARY-PROMPT-7f3a91";
/// The first original a fresh store hands out is the prompt of the first send.
const PROMPT_ORIGINAL_ID: &str = "1";

fn text_turn(text: &str) -> Value {
    json!({"events": [
        {"type": "text_delta", "text": text},
        {"type": "usage", "input_tokens": 11, "output_tokens": 7},
        {"type": "stop", "reason": "end_turn"},
    ]})
}

fn tool_turn(calls: &[(&str, &str, Value)]) -> Value {
    let mut events = Vec::new();
    for (id, name, arguments) in calls {
        events.push(json!({"type": "tool_call_start", "call_id": id, "name": name}));
        events.push(json!({
            "type": "tool_call_arguments_delta",
            "call_id": id,
            "fragment": arguments.to_string(),
        }));
    }
    events.push(json!({"type": "usage", "input_tokens": 5, "output_tokens": 3}));
    events.push(json!({"type": "stop", "reason": "tool_use"}));
    json!({ "events": events })
}

struct Rig {
    data: TempDir,
    cwd: PathBuf,
    _cwd_guard: TempDir,
    hub: Arc<Hub>,
    turns: Arc<Turns>,
    dispatcher: Dispatcher,
    connection: Connection,
    _outbox: Outbox,
    session: SessionId,
    next_id: i64,
}

impl Rig {
    fn new(mode: Mode, script: &[Value]) -> Self {
        Self::with(mode, script, true)
    }

    fn with(mode: Mode, script: &[Value], enabled: bool) -> Self {
        Self::custom(mode, script, enabled, None, |_| {})
    }

    /// A session on `model`, with `tune` applied to the daemon's configuration.
    fn custom(
        mode: Mode,
        script: &[Value],
        enabled: bool,
        model: Option<&str>,
        tune: impl FnOnce(&mut Config),
    ) -> Self {
        let data = tempfile::tempdir().unwrap();
        let cwd_guard = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(cwd_guard.path()).unwrap();
        std::fs::write(cwd.join("a.txt"), "one needle here\n").unwrap();
        // Grants are scoped to a git repo, so the session works in one.
        std::fs::create_dir(cwd.join(".git")).unwrap();
        let script_path = data.path().join("script.json");
        std::fs::write(&script_path, Value::Array(script.to_vec()).to_string()).unwrap();
        let mut models = Config::default();
        models.providers.insert(
            "faux".to_owned(),
            ProviderConfig {
                enabled,
                script: Some(script_path),
                ..ProviderConfig::default()
            },
        );
        tune(&mut models);
        let mut store = Store::open(&data.path().join("store")).unwrap();
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::Faux,
                model: model.map(str::to_owned),
                cwd: cwd.to_string_lossy().into_owned(),
                mode,
            })
            .unwrap();
        let hub = Hub::new(store, QUEUE);
        let turns = Turns::new(
            Arc::clone(&hub),
            TurnConfig {
                binary: None,
                pidfile_dir: data.path().to_path_buf(),
                parent_env: vec![("PATH".into(), "/usr/bin:/bin".into())],
                timing: Timing::default(),
                models,
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
            _cwd_guard: cwd_guard,
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

    async fn bodies(&self, kind: &str) -> Vec<Value> {
        let events = self.events().await;
        let of_kind = events.into_iter().filter(|e| e.kind == kind);
        of_kind
            .map(|e| serde_json::from_str(&e.body).unwrap())
            .collect()
    }

    async fn pending(&self) -> Vec<coda_store::PendingPermission> {
        let session = self.session.clone();
        self.hub
            .with_store(move |store| store.list_pending(&session))
            .await
            .unwrap()
    }

    async fn wait_for_results(&self, count: usize) {
        tokio::time::timeout(WAIT, async {
            while self.bodies("result").await.len() < count {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("fewer than {count} result events were stored in time"));
    }

    /// A second session in the same store.
    async fn other_session(&self) -> SessionId {
        let new = NewSession {
            backend: BackendKind::Faux,
            model: None,
            cwd: self.cwd.to_string_lossy().into_owned(),
            mode: Mode::Default,
        };
        let created = self.hub.with_store_mut(move |s| s.create_session(&new));
        created.await.unwrap()
    }

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
        let params = json!({
            "session_id": self.session, "request_id": request_id, "decision": decision,
        });
        self.call("permission.respond", params).await
    }

    async fn wait_until_idle(&self) {
        tokio::time::timeout(WAIT, async {
            while self.turns.is_running(&self.session) {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("the turn did not finish in time");
    }

    fn db(&self) -> rusqlite::Connection {
        let path = self.data.path().join("store");
        rusqlite::Connection::open(path.join(coda_store::DB_FILE_NAME)).unwrap()
    }
}

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

fn span_text(spans: &Spans) -> String {
    format!("{:?}", spans.all())
}

fn base_tool_calls() -> Value {
    tool_turn(&[
        ("c1", "read", json!({"path": "a.txt"})),
        ("c2", "grep", json!({"pattern": "needle", "path": "."})),
        (
            "c3",
            "edit",
            json!({"path": "a.txt", "old_string": "needle", "new_string": "thread"}),
        ),
        ("c4", "retrieve", json!({"id": PROMPT_ORIGINAL_ID})),
        ("c5", "bash", json!({"command": "touch ran.txt"})),
    ])
}

#[tokio::test]
async fn a_faux_session_completes_a_tool_loop_with_the_base_tools() {
    // Arrange
    let mut rig = Rig::new(
        Mode::AcceptEdits,
        &[base_tool_calls(), text_turn("all done")],
    );

    // Act
    let sent = rig.send("go").await;
    let request = rig.wait_for_pending().await;
    let answered = rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(sent["result"]["turn_id"].is_string());
    assert_eq!(answered["result"]["accepted"], true);
    let finished = rig.bodies("tool_finished").await;
    let tools: Vec<&str> = finished
        .iter()
        .map(|b| b["tool"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["read", "grep", "edit", "retrieve", "bash"]);
    assert!(
        finished.iter().all(|b| b["is_error"] == false),
        "{finished:?}"
    );
    let edited = std::fs::read_to_string(rig.cwd.join("a.txt")).unwrap();
    assert_eq!(edited, "one thread here\n");
    assert!(rig.cwd.join("ran.txt").exists());
    assert_eq!(rig.bodies("turn_stopped").await[0]["condition"], "end_turn");
    let deltas = rig.bodies("stream_delta").await;
    assert_eq!(deltas[0]["text"], "all done");
    assert_eq!(rig.bodies("result").await[0]["is_error"], false);
    assert_eq!(rig.bodies("usage").await.len(), 2);
}

#[tokio::test]
async fn a_denied_tool_call_does_not_run_and_the_turn_still_ends() {
    // Arrange
    let calls = tool_turn(&[("c1", "bash", json!({"command": "touch ran.txt"}))]);
    let mut rig = Rig::new(Mode::Default, &[calls, text_turn("understood")]);

    // Act
    rig.send("go").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "deny").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(!rig.cwd.join("ran.txt").exists());
    assert_eq!(rig.bodies("tool_finished").await[0]["is_error"], true);
    assert_eq!(rig.bodies("turn_stopped").await[0]["condition"], "end_turn");
}

#[tokio::test]
async fn cancel_stops_the_loop_and_expires_the_open_permission_request() {
    // Arrange
    let calls = tool_turn(&[("c1", "bash", json!({"command": "touch ran.txt"}))]);
    let mut rig = Rig::new(Mode::Default, &[calls]);
    rig.send("go").await;
    let request = rig.wait_for_pending().await;

    // Act
    let cancelled = rig
        .call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(cancelled["result"]["cancelled"], true);
    assert_eq!(
        rig.bodies("turn_stopped").await[0]["condition"],
        "cancelled"
    );
    assert_eq!(rig.bodies("result").await[0]["is_error"], false);
    assert_eq!(rig.pending().await.len(), 0);
    let timeouts = rig.bodies("timeout").await;
    assert_eq!(timeouts[0]["request_id"], request);
    assert!(!rig.cwd.join("ran.txt").exists());
}

#[tokio::test]
async fn a_cancel_while_the_send_is_starting_cancels_the_loop_once_it_is_armed() {
    // Arrange
    let rig = Rig::new(Mode::Default, &[]);
    let models = &rig.turns.models;
    let slot = models.reserve(&rig.session).unwrap();
    let token = CancelToken::default();

    // Act
    let cancelled = models.cancel(&rig.session);
    token.reset();
    slot.arm((token.clone(), TurnId::new("t-1")));

    // Assert
    assert!(cancelled);
    assert!(token.is_cancelled());
}

#[tokio::test]
async fn a_cancel_with_no_turn_is_not_remembered_for_the_next_send() {
    // Arrange
    let rig = Rig::new(Mode::Default, &[]);
    let models = &rig.turns.models;
    let cancelled = models.cancel(&rig.session);
    let token = CancelToken::default();

    // Act
    let slot = models.reserve(&rig.session).unwrap();
    slot.arm((token.clone(), TurnId::new("t-1")));

    // Assert
    assert!(!cancelled);
    assert!(!token.is_cancelled());
}

#[tokio::test]
async fn a_disabled_provider_refuses_the_send() {
    // Arrange
    let mut rig = Rig::with(Mode::Default, &[text_turn("hi")], false);

    // Act
    let sent = rig.send("go").await;

    // Assert
    assert_eq!(sent["error"]["code"], BACKEND_UNSUPPORTED);
    assert!(!rig.turns.is_running(&rig.session));
}

#[tokio::test]
async fn the_prompt_canary_is_content_in_events_and_originals_and_nowhere_else() {
    // Arrange
    let (spans, _guard) = install();
    let calls = tool_turn(&[("c1", "read", json!({"path": "a.txt"}))]);
    let mut rig = Rig::new(Mode::Default, &[calls, text_turn("done")]);

    // Act
    rig.send(&format!("hello {PROMPT_CANARY}")).await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let db = rig.db();
    assert!(
        dump(&db, "events").contains(PROMPT_CANARY),
        "events control"
    );
    assert!(
        dump(&db, "originals").contains(PROMPT_CANARY),
        "originals control"
    );
    assert!(!dump(&db, "usage").is_empty(), "usage control");
    for table in ["event_log", "usage", "turns"] {
        assert!(
            !dump(&db, table).contains(PROMPT_CANARY),
            "{table} holds the prompt"
        );
    }
    assert!(!span_text(&spans).contains(PROMPT_CANARY));
    assert_eq!(spans.named(PROVIDER_CALL_SPAN).len(), 2);
    assert_eq!(spans.named(TOOL_CALL_SPAN).len(), 1);
}

/// The shell's pid, once the command has written it.
async fn shell_pid(cwd: &std::path::Path) -> String {
    let pid_file = cwd.join("shell.pid");
    tokio::time::timeout(WAIT, async {
        loop {
            match std::fs::read_to_string(&pid_file) {
                Ok(pid) if pid.ends_with('\n') => return pid.trim().to_owned(),
                _ => tokio::time::sleep(POLL).await,
            }
        }
    })
    .await
    .expect("the command did not start in time")
}

fn process_alive(pid: &str) -> bool {
    let probe = std::process::Command::new("kill")
        .args(["-0", pid])
        .output();
    probe.unwrap().status.success()
}

#[tokio::test]
async fn shutdown_kills_a_running_bash_group_and_closes_its_turn() {
    // Arrange
    let command = "echo $$ > shell.pid; sleep 300";
    let calls = tool_turn(&[("c1", "bash", json!({ "command": command }))]);
    let mut rig = Rig::new(Mode::Default, &[calls]);
    rig.send("go").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    let pid = shell_pid(&rig.cwd).await;
    assert!(process_alive(&pid), "the command control");

    // Act
    rig.turns.shutdown().await;

    // Assert
    rig.wait_for("result").await;
    assert!(!process_alive(&pid));
    assert!(!rig.turns.is_running(&rig.session));
}

#[tokio::test]
async fn a_turn_that_outlasts_the_grace_period_is_closed_as_failed() {
    // Arrange
    let command = "echo $$ > shell.pid; sleep 300";
    let calls = tool_turn(&[("c1", "bash", json!({ "command": command }))]);
    let mut rig = Rig::new(Mode::Default, &[calls]);
    rig.send("go").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    shell_pid(&rig.cwd).await;

    // Act
    rig.turns.models.shutdown_within(Duration::ZERO).await;

    // Assert
    let results = rig.bodies("result").await;
    assert_eq!(results[0]["is_error"], true, "{results:?}");
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn the_api_key_is_read_from_the_live_environment_on_every_call() {
    // Arrange
    let rig = Rig::new(Mode::Default, &[]);
    let mut config = Config::default();
    config.providers.insert(
        "anthropic".to_owned(),
        ProviderConfig {
            enabled: true,
            api_key_env: Some("CODA_TEST_KEY".to_owned()),
            ..ProviderConfig::default()
        },
    );
    let key = Arc::new(std::sync::Mutex::new(Some("first".to_owned())));
    let live = Arc::clone(&key);
    let reader: EnvReader = Arc::new(move |_| live.lock().unwrap().clone());
    let hub = Arc::clone(&rig.hub);
    let sessions = ModelSessions::new(
        Arc::clone(&hub),
        Brokers::new(hub, Vec::new()),
        config,
        Vec::new(),
        reader,
    );
    let record = SessionRecord {
        id: rig.session.clone(),
        backend: BackendKind::Anthropic,
        model: None,
        cwd: rig.cwd.to_string_lossy().into_owned(),
        mode: Mode::Default,
        created_at: 0,
    };
    let provider = sessions.provider(&record).unwrap();
    let request = ModelRequest {
        system: String::new(),
        tools: Vec::new(),
        messages: Vec::new(),
    };

    // Act
    *key.lock().unwrap() = None;
    let outcome = tokio::time::timeout(WAIT, async {
        match provider.stream(request).await {
            Ok(mut stream) => stream.next().await.and_then(Result::err),
            Err(e) => Some(e),
        }
    })
    .await;

    // Assert
    let message = format!("{:?}", outcome.expect("the call did not return in time"));
    assert!(
        message.contains("CODA_TEST_KEY holding the API key is not set"),
        "the provider kept the key it first saw: {message}"
    );
}

#[tokio::test]
async fn a_panic_in_the_session_task_ends_the_turn_and_the_next_send_starts_over() {
    // Arrange
    let mut rig = Rig::custom(Mode::Default, &[], true, Some("panic"), |_| {});

    // Act
    rig.send("one").await;
    rig.wait_for_results(1).await;
    rig.wait_until_idle().await;
    let idle_loops = lock(&rig.turns.models.loops).len();
    let after_first = (
        rig.pending().await.len(),
        idle_loops,
        rig.bodies(CONVERSATION_RESET_KIND).await.len(),
    );
    rig.send("two").await;
    rig.wait_for_results(2).await;

    // Assert
    assert_eq!(rig.bodies("result").await[0]["is_error"], true);
    assert_eq!(after_first, (0, 0, 0));
    assert_eq!(rig.bodies(CONVERSATION_RESET_KIND).await.len(), 1);
}

#[test]
fn the_oldest_conversation_is_evicted_once_more_than_the_maximum_are_kept() {
    // Arrange
    let mut kept = Vec::new();
    let id = |n: &str| SessionId::new(n);

    // Act
    let evictions: Vec<Option<SessionId>> = ["1", "2", "3"]
        .into_iter()
        .map(|n| keep_recent(&mut kept, id(n), n, 2))
        .collect();

    // Assert
    assert_eq!(evictions, [None, None, Some(id("1"))]);
    let names: Vec<_> = kept.iter().map(|(_, n)| *n).collect();
    assert_eq!(names, ["2", "3"]);
}

#[tokio::test]
async fn only_a_rebuilt_session_with_history_stores_a_conversation_reset() {
    // Arrange
    let mut rig = Rig::new(Mode::Default, &[text_turn("one"), text_turn("two")]);

    // Act
    rig.send("first").await;
    rig.wait_for_results(1).await;
    rig.wait_until_idle().await;
    let after_first = rig.bodies(CONVERSATION_RESET_KIND).await.len();
    lock(&rig.turns.models.loops).clear();
    rig.send("second").await;
    rig.wait_for_results(2).await;

    // Assert
    assert_eq!(after_first, 0, "a new session has nothing to reset");
    assert_eq!(rig.bodies(CONVERSATION_RESET_KIND).await.len(), 1);
}

#[tokio::test]
async fn a_gate_that_errors_never_runs_the_tool() {
    // Arrange
    let calls = tool_turn(&[("c1", "bash", json!({"command": "touch ran.txt"}))]);
    let mut rig = Rig::new(Mode::Default, &[calls, text_turn("ok")]);
    rig.db()
        .execute_batch("DROP TABLE permission_grants")
        .unwrap();

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(!rig.cwd.join("ran.txt").exists());
    assert_eq!(rig.bodies("tool_finished").await[0]["is_error"], true);
}

#[tokio::test]
async fn an_answer_naming_another_session_is_refused_and_leaves_the_request_open() {
    // Arrange
    let calls = tool_turn(&[("c1", "bash", json!({"command": "touch ran.txt"}))]);
    let mut rig = Rig::new(Mode::Default, &[calls, text_turn("ok")]);
    let other = rig.other_session().await;
    rig.send("go").await;
    let request = rig.wait_for_pending().await;

    // Act
    let params = json!({"session_id": other, "request_id": request, "decision": "allow_once"});
    let refused = rig.call("permission.respond", params).await;

    // Assert
    assert_eq!(refused["result"]["accepted"], false);
    assert_eq!(rig.pending().await.len(), 1);
    assert!(!rig.cwd.join("ran.txt").exists());
    let own = rig.respond(&request, "deny").await;
    assert_eq!(own["result"]["accepted"], true, "the owner control");
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn an_unanswered_prompt_is_denied_after_the_default_deny_timeout() {
    // Arrange
    let calls = tool_turn(&[("c1", "bash", json!({"command": "touch ran.txt"}))]);
    let script = [calls, text_turn("ok")];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, |config| {
        config.timing.permission_default_deny = Duration::from_millis(100);
    });

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(!rig.cwd.join("ran.txt").exists());
    assert_eq!(rig.bodies("tool_finished").await[0]["is_error"], true);
    assert_eq!(rig.pending().await.len(), 0);
}

#[tokio::test]
async fn a_grant_covers_only_the_exact_command_in_a_model_session() {
    // Arrange
    let granted = "touch ok.txt";
    let calls = tool_turn(&[
        ("c1", "bash", json!({"command": granted})),
        (
            "c2",
            "bash",
            json!({"command": format!("{granted}; touch evil.txt")}),
        ),
    ]);
    let mut rig = Rig::new(Mode::Default, &[calls, text_turn("ok")]);
    let repo = rig.cwd.to_string_lossy().into_owned();
    let (tool, pattern) = ("bash".to_owned(), granted.to_owned());
    rig.hub
        .add_grant(repo.clone(), tool, repo, pattern)
        .await
        .unwrap();

    // Act
    rig.send("go").await;
    let request = rig.wait_for_pending().await;

    // Assert
    assert!(
        rig.cwd.join("ok.txt").exists(),
        "the granted command control"
    );
    assert!(!rig.cwd.join("evil.txt").exists());
    assert_eq!(rig.pending().await.len(), 1);
    rig.respond(&request, "deny").await;
    rig.wait_until_idle().await;
    assert!(!rig.cwd.join("evil.txt").exists());
}

#[tokio::test]
async fn the_tool_output_canary_is_content_in_originals_and_nowhere_else() {
    // Arrange
    let canary = "CANARY-TOOL-OUTPUT-5c2e84";
    let (spans, _guard) = install();
    let calls = tool_turn(&[("c1", "read", json!({"path": "big.txt"}))]);
    let mut rig = Rig::new(Mode::Default, &[calls, text_turn("done")]);
    let big = format!("{canary}\n{}", "padding line\n".repeat(3000));
    std::fs::write(rig.cwd.join("big.txt"), big).unwrap();

    // Act
    rig.send("read it").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let db = rig.db();
    assert!(dump(&db, "originals").contains(canary), "originals control");
    assert!(!dump(&db, "usage").is_empty(), "usage control");
    for table in ["events", "event_log", "usage", "turns"] {
        assert!(!dump(&db, table).contains(canary), "{table} holds the file");
    }
    assert!(!span_text(&spans).contains(canary));
    assert_eq!(rig.bodies("tool_finished").await[0]["is_error"], false);
}

#[tokio::test]
async fn the_tool_argument_canary_is_content_in_originals_and_nowhere_else() {
    // Arrange
    let canary = "CANARY-TOOL-ARGUMENT-0b7d13";
    let edit = json!({"path": "a.txt", "old_string": "needle", "new_string": canary});
    let calls = tool_turn(&[("c1", "edit", edit)]);
    let mut rig = Rig::new(Mode::AcceptEdits, &[calls, text_turn("done")]);

    // Act
    rig.send("change it").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let db = rig.db();
    assert!(dump(&db, "originals").contains(canary), "originals control");
    for table in ["events", "event_log", "usage", "turns"] {
        assert!(!dump(&db, table).contains(canary), "{table} holds the file");
    }
    let stored = rig.bodies("assistant_message").await;
    assert!(
        stored[0]["tool_calls"][0]["arguments"].is_string(),
        "{stored:?}"
    );
}

fn indexed_symbols(rig: &Rig, name: &str) -> i64 {
    rig.db()
        .query_row(
            "SELECT COUNT(*) FROM symbols WHERE name = ?1",
            [name],
            |row| row.get(0),
        )
        .unwrap()
}

fn write_source(cwd: &std::path::Path, name: &str) {
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(cwd.join("src/lib.rs"), format!("pub fn {name}() {{}}\n")).unwrap();
}

#[tokio::test]
async fn starting_a_session_indexes_the_workspace_without_any_edit() {
    // Arrange
    let mut rig = Rig::new(Mode::Default, &[text_turn("hi")]);
    write_source(&rig.cwd, "indexed_at_start");
    let before = indexed_symbols(&rig, "indexed_at_start");

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(before, 0);
    assert_eq!(indexed_symbols(&rig, "indexed_at_start"), 1);
    assert_eq!(rig.bodies("result").await[0]["is_error"], false);
}

#[tokio::test]
async fn a_failed_index_refresh_does_not_fail_the_session_start() {
    // Arrange
    let mut rig = Rig::new(Mode::Default, &[text_turn("hi")]);
    write_source(&rig.cwd, "never_indexed");
    // Without its tables the refresh cannot write, so it fails after the walk.
    rig.db()
        .execute_batch("DROP TABLE symbols_fts; DROP TABLE symbols; DROP TABLE index_files;")
        .unwrap();

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(rig.bodies("result").await[0]["is_error"], false);
    assert_eq!(rig.bodies("stream_delta").await[0]["text"], "hi");
}

/// Files that are costly to parse yet hold few symbols, so the walk dominates
/// the time and the writes that follow stay short.
fn write_large_workspace(cwd: &std::path::Path) {
    let body = "    let value = (1 + 2) * 3;\n".repeat(4000);
    for file in 0..6 {
        let source = format!("pub fn f{file}() {{\n{body}}}\n");
        std::fs::write(cwd.join(format!("m{file}.rs")), source).unwrap();
    }
}

/// The time a full walk of `cwd` takes on this machine.
fn time_a_walk(cwd: &std::path::Path) -> Duration {
    let scratch = tempfile::tempdir().unwrap();
    let store = std::sync::Mutex::new(Store::open(&scratch.path().join("store")).unwrap());
    let started = std::time::Instant::now();
    coda_core::tools::refresh_index(&store, cwd);
    started.elapsed()
}

#[tokio::test]
async fn a_store_user_is_not_blocked_while_the_index_walk_runs() {
    // Arrange
    let mut rig = Rig::new(Mode::Default, &[text_turn("hi")]);
    write_large_workspace(&rig.cwd);
    let walk = time_a_walk(&rig.cwd);
    assert!(
        walk > Duration::from_millis(100),
        "{walk:?} is too short to measure"
    );
    let session = rig.session.clone();
    let hub = Arc::clone(&rig.hub);
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = Arc::clone(&done);
    let user = tokio::spawn(async move {
        let mut longest = Duration::ZERO;
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let started = std::time::Instant::now();
            let session = session.clone();
            hub.with_store(move |store| store.load_events(&session, 0, 10))
                .await
                .unwrap();
            longest = longest.max(started.elapsed());
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        longest
    });

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    let longest = user.await.unwrap();

    // Assert
    assert!(
        longest < walk / 2,
        "a store call waited {longest:?} of a {walk:?} walk"
    );
    assert_eq!(indexed_symbols(&rig, "f5"), 1);
}

mod switch;
