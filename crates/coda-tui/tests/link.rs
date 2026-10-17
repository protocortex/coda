// SPDX-License-Identifier: AGPL-3.0-only

//! The daemon link against a real server with a store, no live backend.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::time::Duration;

use coda_config::timing::Timing;
use coda_daemon::server::{Server, ServerConfig};
use coda_daemon::subscribe::Hub;
use coda_protocol::types::{PermissionDecision, RequestId, SessionId, SessionInfo};
use coda_store::Store;
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
        // macOS limits socket paths to 104 bytes, so keep them under /tmp.
        let dir = tempfile::Builder::new()
            .prefix("ct")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::set_permissions(data.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let uid = std::fs::metadata(dir.path()).unwrap().uid();
        let hub = Hub::new(Store::open(data.path()).unwrap(), QUEUE);
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
            LinkConfig::default(),
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
