// SPDX-License-Identifier: AGPL-3.0-only

//! Model-API sessions end to end through the dispatcher, against the faux provider.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use coda_config::config::{Config, ProviderConfig};
use coda_protocol::jsonrpc::Response;
use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::{NewSession, Store, StoredEvent};
use rusqlite::types::ValueRef;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::dispatch::Dispatcher;
use crate::state::DaemonState;
use crate::subscribe::{Connection, Hub, Outbox};
use crate::trace::capture::{Spans, install};
use crate::turn::{BACKEND_UNSUPPORTED, TurnConfig, Turns};
use crate::watchdog::Timing;
use coda_core::turn::{PROVIDER_CALL_SPAN, TOOL_CALL_SPAN};

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
        let data = tempfile::tempdir().unwrap();
        let cwd_guard = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(cwd_guard.path()).unwrap();
        std::fs::write(cwd.join("a.txt"), "one needle here\n").unwrap();
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
        let mut store = Store::open(&data.path().join("store")).unwrap();
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::Faux,
                model: None,
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
