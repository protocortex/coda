// SPDX-License-Identifier: AGPL-3.0-only

//! The client against a real daemon server on a temporary socket.

mod common;

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;

use coda_client::{Client, ClientError};
use coda_daemon::server::{Server, ServerConfig};
use coda_daemon::subscribe::Hub;
use coda_protocol::jsonrpc::METHOD_NOT_FOUND;
use coda_protocol::methods::{DAEMON_PING, PingParams, PingResult};
use coda_protocol::types::{BackendKind, Mode, SessionId, TurnId};
use coda_store::{EventRecord, NewSession, Store};
use common::{short_dir, timing, within};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

struct TestDaemon {
    socket: PathBuf,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<()>,
    _dir: TempDir,
}

impl TestDaemon {
    fn start(hub: Option<Arc<Hub>>) -> Self {
        let dir = short_dir();
        let uid = std::fs::metadata(dir.path()).unwrap().uid();
        let config = ServerConfig::new(dir.path().join("s.sock"), uid);
        let socket = config.socket_path.clone();
        let mut bound = Server::bind(config).unwrap();
        if let Some(hub) = hub {
            bound = bound.with_hub(hub);
        }
        let (shutdown, rx) = oneshot::channel();
        let handle = tokio::spawn(bound.serve(async {
            let _ = rx.await;
        }));
        Self {
            socket,
            shutdown,
            handle,
            _dir: dir,
        }
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        within(self.handle).await.unwrap();
    }
}

struct Fixture {
    hub: Arc<Hub>,
    session: SessionId,
    turn: TurnId,
    _data: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let mut store = Store::open(&data.path().join("data")).unwrap();
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::ClaudeCode,
                model: None,
                cwd: "/work".to_owned(),
                mode: Mode::default(),
            })
            .unwrap();
        let turn = store.begin_turn(&session).unwrap();
        Self {
            hub: Hub::new(store, 64),
            session,
            turn,
            _data: data,
        }
    }

    async fn publish(&self, body: &str) {
        let record = EventRecord {
            kind: "stream_delta".to_owned(),
            body: body.to_owned(),
        };
        self.hub
            .publish(&self.session, &self.turn, record)
            .await
            .unwrap();
    }
}

async fn next_seq(subscription: &mut coda_client::Subscription) -> u64 {
    let notification = within(subscription.next()).await.expect("stream ended");
    notification.params.unwrap()["seq"].as_u64().unwrap()
}

#[tokio::test]
async fn unchecked_connection_answers_requests_without_a_handshake() {
    // Arrange
    let daemon = TestDaemon::start(None);
    let client = Client::connect_unchecked(&daemon.socket, timing())
        .await
        .unwrap();

    // Act
    let reply: Result<PingResult, ClientError> =
        within(client.request(DAEMON_PING, &PingParams {})).await;

    // Assert
    assert_eq!(reply.unwrap(), PingResult {});
    daemon.stop().await;
}

#[tokio::test]
async fn connect_then_ping_round_trips() {
    // Arrange
    let daemon = TestDaemon::start(None);

    // Act
    let client = within(Client::connect(&daemon.socket, timing()))
        .await
        .unwrap();
    let pong: PingResult = within(client.request(DAEMON_PING, &PingParams {}))
        .await
        .unwrap();

    // Assert
    assert_eq!(pong, PingResult {});
    daemon.stop().await;
}

#[tokio::test]
async fn unknown_method_surfaces_a_typed_rpc_error() {
    // Arrange
    let daemon = TestDaemon::start(None);
    let client = within(Client::connect(&daemon.socket, timing()))
        .await
        .unwrap();

    // Act
    let result: Result<Value, _> = within(client.request("no.such.method", &PingParams {})).await;

    // Assert
    match result {
        Err(ClientError::Rpc { code, .. }) => assert_eq!(code, METHOD_NOT_FOUND),
        other => panic!("expected an Rpc error, got {other:?}"),
    }
    daemon.stop().await;
}

#[tokio::test]
async fn subscribing_from_a_cursor_yields_each_missed_event_once() {
    // Arrange
    let fixture = Fixture::new();
    for n in 1..=5 {
        fixture.publish(&format!("e{n}")).await;
    }
    let daemon = TestDaemon::start(Some(Arc::clone(&fixture.hub)));
    let client = within(Client::connect(&daemon.socket, timing()))
        .await
        .unwrap();

    // Act
    let mut subscription = within(client.subscribe(&fixture.session, Some(3)))
        .await
        .unwrap();
    fixture.publish("e6").await;

    // Assert
    let mut seqs = Vec::new();
    for _ in 0..3 {
        seqs.push(next_seq(&mut subscription).await);
    }
    assert_eq!(seqs, vec![4, 5, 6]);
    daemon.stop().await;
}

#[tokio::test]
async fn resume_after_a_reconnect_continues_from_the_last_seen_event() {
    // Arrange
    let fixture = Fixture::new();
    for n in 1..=5 {
        fixture.publish(&format!("e{n}")).await;
    }
    let daemon = TestDaemon::start(Some(Arc::clone(&fixture.hub)));
    let first = within(Client::connect(&daemon.socket, timing()))
        .await
        .unwrap();
    let mut subscription = within(first.subscribe(&fixture.session, Some(0)))
        .await
        .unwrap();
    assert_eq!(next_seq(&mut subscription).await, 1);
    assert_eq!(next_seq(&mut subscription).await, 2);
    drop(first);

    // Act
    let second = within(Client::connect(&daemon.socket, timing()))
        .await
        .unwrap();
    let mut resumed = within(second.resume(&subscription)).await.unwrap();

    // Assert
    let mut seqs = Vec::new();
    for _ in 0..3 {
        seqs.push(next_seq(&mut resumed).await);
    }
    assert_eq!(seqs, vec![3, 4, 5]);
    daemon.stop().await;
}

#[tokio::test]
async fn server_shutdown_surfaces_disconnected() {
    // Arrange
    let daemon = TestDaemon::start(None);
    let client = within(Client::connect(&daemon.socket, timing()))
        .await
        .unwrap();

    // Act
    daemon.stop().await;
    let result: Result<PingResult, _> = within(client.request(DAEMON_PING, &PingParams {})).await;

    // Assert
    assert!(
        matches!(result, Err(ClientError::Disconnected)),
        "got {result:?}"
    );
}
