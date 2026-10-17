// SPDX-License-Identifier: AGPL-3.0-only

//! The daemon link against a real server with a store, no live backend.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::Duration;

use coda_config::timing::Timing;
use coda_daemon::server::{Server, ServerConfig};
use coda_daemon::subscribe::Hub;
use coda_protocol::types::{
    BackendKind, Mode, PermissionDecision, RequestId, SessionId, SessionInfo,
};
use coda_store::{EventRecord, NewSession, Store};
use coda_tui::app::LinkEvent;
use coda_tui::permission::PermissionAnswer;
use coda_tui::sessions::{LinkCommand, LinkConfig, run_link};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

const WAIT: Duration = Duration::from_secs(5);
const QUEUE: usize = 64;

struct Rig {
    dir: tempfile::TempDir,
    _data: tempfile::TempDir,
    _stop: oneshot::Sender<()>,
}

impl Rig {
    fn start() -> Self {
        Self::start_seeded(|_| {})
    }

    /// Starts a daemon whose store `seed` filled first, like one that restarted.
    fn start_seeded(seed: impl FnOnce(&mut Store)) -> Self {
        // macOS limits socket paths to 104 bytes, so keep them under /tmp.
        let dir = tempfile::Builder::new()
            .prefix("ct")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let uid = std::fs::metadata(dir.path()).unwrap().uid();
        let mut store = Store::open(data.path()).unwrap();
        seed(&mut store);
        let hub = Hub::new(store, QUEUE);
        let bound = Server::bind(ServerConfig::new(dir.path().join("s.sock"), uid))
            .unwrap()
            .with_hub(hub);
        let (stop, rx) = oneshot::channel();
        tokio::spawn(bound.serve(async {
            let _ = rx.await;
        }));
        Self {
            dir,
            _data: data,
            _stop: stop,
        }
    }

    fn link(&self) -> Link {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        tokio::spawn(run_link(
            self.dir.path().join("s.sock"),
            Timing::for_tests(),
            LinkConfig {
                backend: BackendKind::Faux,
                ..LinkConfig::default()
            },
            command_rx,
            event_tx,
        ));
        Link { commands, events }
    }
}

struct Link {
    commands: UnboundedSender<LinkCommand>,
    events: UnboundedReceiver<LinkEvent>,
}

impl Link {
    /// Skips events until one satisfies `pick`.
    async fn wait_for<T>(&mut self, pick: impl Fn(LinkEvent) -> Option<T>) -> T {
        tokio::time::timeout(WAIT, async {
            while let Some(event) = self.events.recv().await {
                if let Some(found) = pick(event) {
                    return found;
                }
            }
            panic!("the link ended");
        })
        .await
        .expect("timed out waiting for a link event")
    }

    async fn started(&mut self) -> SessionInfo {
        self.wait_for(|event| match event {
            LinkEvent::SessionStarted(info) => Some(info),
            _ => None,
        })
        .await
    }

    async fn notice(&mut self) -> String {
        self.wait_for(|event| match event {
            LinkEvent::Notice(text) => Some(text),
            _ => None,
        })
        .await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn new_and_resume_switch_between_sessions() {
    // Arrange
    let rig = Rig::start();
    let mut link = rig.link();
    let first = link.started().await;

    // Act
    link.commands.send(LinkCommand::New).unwrap();
    let second = link.started().await;
    link.commands
        .send(LinkCommand::Resume(first.id.clone()))
        .unwrap();
    let back = link.started().await;

    // Assert
    assert_ne!(first.id, second.id);
    assert_eq!(back.id, first.id);
}

#[tokio::test(flavor = "multi_thread")]
async fn resuming_an_unknown_session_and_stale_answers_show_notices() {
    // Arrange
    let rig = Rig::start();
    let mut link = rig.link();
    link.started().await;

    // Act
    link.commands
        .send(LinkCommand::Resume(SessionId::new("nope")))
        .unwrap();
    let unknown = link.notice().await;
    link.commands
        .send(LinkCommand::Respond(PermissionAnswer {
            session_id: SessionId::new("1"),
            request_id: RequestId::new("none"),
            decision: PermissionDecision::Deny,
        }))
        .unwrap();
    let stale = link.notice().await;

    // Assert
    assert!(unknown.contains("could not reopen"), "{unknown}");
    assert!(stale.contains("answer the permission request"), "{stale}");
}

/// A stored session of this process's working directory, with one user message.
fn seed_session(store: &mut Store, text: &str) -> SessionId {
    let cwd = std::env::current_dir()
        .unwrap()
        .canonicalize()
        .unwrap()
        .display()
        .to_string();
    let id = store
        .create_session(&NewSession {
            backend: BackendKind::Faux,
            model: None,
            cwd,
            mode: Mode::Default,
        })
        .unwrap();
    let turn = store.begin_turn(&id).unwrap();
    let record = EventRecord {
        kind: "user_message".to_owned(),
        body: serde_json::json!({ "text": text }).to_string(),
    };
    store.append_event(&id, &turn, &record).unwrap();
    id
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_resumes_the_latest_session_of_the_cwd_and_replays_the_user_line() {
    // Arrange
    let mut seeded = Vec::new();
    let rig = Rig::start_seeded(|store| {
        seed_session(store, "older question");
        seeded.push(seed_session(store, "what changed?"));
    });

    // Act
    let mut link = rig.link();
    let started = link.started().await;
    let notice = link.notice().await;
    let first_line = link
        .wait_for(|event| match event {
            LinkEvent::UserMessage(text) => Some(text),
            _ => None,
        })
        .await;

    // Assert
    assert_eq!(started.id, seeded[0]);
    assert!(
        notice.contains(&format!("resumed session {}", seeded[0])),
        "{notice}"
    );
    assert_eq!(first_line, "what changed?");
}

#[tokio::test(flavor = "multi_thread")]
async fn new_after_a_restart_starts_a_fresh_session() {
    // Arrange
    let mut seeded = Vec::new();
    let rig = Rig::start_seeded(|store| seeded.push(seed_session(store, "old")));
    let mut link = rig.link();
    link.started().await;

    // Act
    link.commands.send(LinkCommand::New).unwrap();
    let fresh = link.started().await;

    // Assert
    assert_ne!(fresh.id, seeded[0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn listing_sessions_reports_the_daemon_sessions() {
    // Arrange
    let mut seeded = Vec::new();
    let rig = Rig::start_seeded(|store| seeded.push(seed_session(store, "old")));
    let mut link = rig.link();
    link.started().await;

    // Act
    link.commands.send(LinkCommand::ListSessions).unwrap();
    let listed = link
        .wait_for(|event| match event {
            LinkEvent::Sessions(sessions) => Some(sessions),
            _ => None,
        })
        .await;

    // Assert
    let ids: Vec<_> = listed.into_iter().map(|s| s.id).collect();
    assert_eq!(ids, seeded);
}
