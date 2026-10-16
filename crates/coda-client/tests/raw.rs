// SPDX-License-Identifier: AGPL-3.0-only

//! The client against a scripted server, for behaviour a real daemon never shows.

mod common;

use std::path::PathBuf;
use std::time::Duration;

use coda_client::{Client, ClientError};
use coda_config::timing::Timing;
use coda_protocol::codec::{FrameDecoder, MAX_FRAME_BYTES, decode_message, encode_frame};
use coda_protocol::jsonrpc::{Id, Message};
use coda_protocol::methods::{DAEMON_STATUS, PROTOCOL_VERSION};
use coda_protocol::types::SessionId;
use common::{short_dir, timing, within};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

struct RawServer {
    listener: UnixListener,
    socket: PathBuf,
    _dir: TempDir,
}

struct RawConn {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl RawServer {
    fn bind() -> Self {
        let dir = short_dir();
        let socket = dir.path().join("r.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        Self {
            listener,
            socket,
            _dir: dir,
        }
    }

    /// Accepts one client and answers its `initialize` with `protocol_version`.
    async fn accept(&self, protocol_version: u32) -> RawConn {
        let (stream, _) = within(self.listener.accept()).await.unwrap();
        let mut conn = RawConn {
            stream,
            decoder: FrameDecoder::new(),
        };
        let (id, method, _) = conn.recv_request().await;
        assert_eq!(method, "initialize");
        conn.reply(
            &id,
            json!({"protocol_version": protocol_version, "daemon_version": "test"}),
        )
        .await;
        conn
    }

    async fn connect(&self, timing: Timing) -> (Client, RawConn) {
        let (client, conn) = tokio::join!(
            Client::connect(&self.socket, timing),
            self.accept(PROTOCOL_VERSION)
        );
        (client.unwrap(), conn)
    }
}

impl RawConn {
    async fn recv(&mut self) -> Message {
        within(async {
            loop {
                if let Some(frame) = self.decoder.next_frame().unwrap() {
                    return decode_message(&frame).unwrap();
                }
                let mut buf = [0u8; 4096];
                let n = self.stream.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "client closed the connection");
                self.decoder.push(&buf[..n]).unwrap();
            }
        })
        .await
    }

    async fn recv_request(&mut self) -> (Id, String, Option<Value>) {
        match self.recv().await {
            Message::Request(r) => (r.id, r.method, r.params),
            other => panic!("expected a request, got {other:?}"),
        }
    }

    async fn send(&mut self, value: Value) {
        let frame = encode_frame(value.to_string().as_bytes()).unwrap();
        self.stream.write_all(&frame).await.unwrap();
    }

    async fn reply(&mut self, id: &Id, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
            .await;
    }

    async fn event(&mut self, session: &SessionId, seq: u64) {
        self.send(json!({"jsonrpc": "2.0", "method": "session.event",
            "params": {"session_id": session, "seq": seq, "kind": "k", "body": "b"}}))
            .await;
    }

    async fn permission(&mut self, session: &SessionId, request: &str) {
        self.send(json!({"jsonrpc": "2.0", "method": "permission.request",
            "params": {"session_id": session, "request_id": request,
                       "repo_path": "/r", "command": "c"}}))
            .await;
    }

    /// Answers a `session.subscribe` request with the daemon's ack.
    async fn ack_subscribe(&mut self) {
        let (id, method, _) = self.recv_request().await;
        assert_eq!(method, "session.subscribe");
        self.reply(&id, json!({"subscribed": true})).await;
    }
}

#[tokio::test]
async fn initialize_version_mismatch_is_surfaced() {
    // Arrange
    let server = RawServer::bind();
    let other_version = PROTOCOL_VERSION + 1;

    // Act
    let (result, _conn) = tokio::join!(
        Client::connect(&server.socket, timing()),
        server.accept(other_version)
    );

    // Assert
    match result {
        Err(ClientError::ProtocolMismatch { client, daemon }) => {
            assert_eq!((client, daemon), (PROTOCOL_VERSION, other_version));
        }
        other => panic!("expected ProtocolMismatch, got {:?}", other.err()),
    }
}

#[tokio::test]
async fn unchecked_connection_skips_the_handshake_with_a_foreign_daemon() {
    // Arrange
    let server = RawServer::bind();
    let other_version = PROTOCOL_VERSION + 1;
    let serve = async {
        let (stream, _) = within(server.listener.accept()).await.unwrap();
        let mut conn = RawConn {
            stream,
            decoder: FrameDecoder::new(),
        };
        let (id, method, _) = conn.recv_request().await;
        assert_eq!(method, DAEMON_STATUS);
        conn.reply(&id, json!({"protocol_version": other_version}))
            .await;
    };

    // Act
    let (reply, ()) = tokio::join!(
        async {
            let client = Client::connect_unchecked(&server.socket, timing()).await?;
            client.request::<_, Value>(DAEMON_STATUS, &json!({})).await
        },
        serve
    );

    // Assert
    assert_eq!(reply.unwrap()["protocol_version"], json!(other_version));
}

#[tokio::test]
async fn concurrent_requests_are_matched_to_their_own_responses() {
    // Arrange
    let server = RawServer::bind();
    let (client, mut conn) = server.connect(timing()).await;

    // Act
    let server_side = async {
        let first = conn.recv_request().await;
        let second = conn.recv_request().await;
        // Answer in reverse order so only id correlation can match them up.
        for (id, method, _) in [second, first] {
            conn.reply(&id, json!({ "echo": method })).await;
        }
    };
    let empty = json!({});
    let (a, b, ()) = tokio::join!(
        client.request::<_, Value>("alpha", &empty),
        client.request::<_, Value>("beta", &empty),
        server_side
    );

    // Assert
    assert_eq!(a.unwrap(), json!({"echo": "alpha"}));
    assert_eq!(b.unwrap(), json!({"echo": "beta"}));
}

#[tokio::test]
async fn replayed_duplicate_events_are_delivered_once() {
    // Arrange
    let server = RawServer::bind();
    let (client, mut conn) = server.connect(timing()).await;
    let session = SessionId::new("s1");
    let subscribing = client.subscribe(&session, Some(1));

    // Act
    let (subscription, ()) = tokio::join!(subscribing, conn.ack_subscribe());
    let mut subscription = subscription.unwrap();
    for seq in [1, 2, 2, 3, 2, 4] {
        conn.event(&session, seq).await;
    }

    // Assert
    let mut seqs = Vec::new();
    for _ in 0..3 {
        let n = within(subscription.next()).await.unwrap();
        seqs.push(n.params.unwrap()["seq"].as_u64().unwrap());
    }
    assert_eq!(seqs, vec![2, 3, 4]);
}

#[tokio::test]
async fn duplicate_permission_requests_are_delivered_once() {
    // Arrange
    let server = RawServer::bind();
    let (client, mut conn) = server.connect(timing()).await;
    let session = SessionId::new("s1");
    let (subscription, ()) = tokio::join!(client.subscribe(&session, None), conn.ack_subscribe());
    let mut subscription = subscription.unwrap();

    // Act
    for request in ["r1", "r1", "r2"] {
        conn.permission(&session, request).await;
    }

    // Assert
    let mut requests = Vec::new();
    for _ in 0..2 {
        let n = within(subscription.next()).await.unwrap();
        assert_eq!(n.method, "permission.request");
        requests.push(n.params.unwrap()["request_id"].as_str().unwrap().to_owned());
    }
    assert_eq!(requests, vec!["r1", "r2"]);
}

#[tokio::test]
async fn request_times_out_when_the_server_never_answers() {
    // Arrange
    let server = RawServer::bind();
    let short = Timing {
        idle_timeout: Duration::from_millis(50),
        ..Timing::for_tests()
    };
    let (client, mut conn) = server.connect(short).await;

    // Act
    let result: Result<Value, _> = client.request("silent", &json!({})).await;
    let _ = conn.recv_request().await;

    // Assert
    match result {
        Err(ClientError::Timeout { method }) => assert_eq!(method, "silent"),
        other => panic!("expected a timeout, got {other:?}"),
    }
}

#[tokio::test]
async fn connection_loss_fails_pending_requests_and_ends_subscriptions() {
    // Arrange
    let server = RawServer::bind();
    let (client, mut conn) = server.connect(timing()).await;
    let session = SessionId::new("s1");
    let (subscription, ()) = tokio::join!(client.subscribe(&session, None), conn.ack_subscribe());
    let mut subscription = subscription.unwrap();

    // Act
    let empty = json!({});
    let pending = client.request::<_, Value>("hang", &empty);
    let drop_server = async {
        let _ = conn.recv_request().await;
        drop(conn);
    };
    let (result, ()) = tokio::join!(pending, drop_server);

    // Assert
    assert!(
        matches!(result, Err(ClientError::Disconnected)),
        "got {result:?}"
    );
    assert!(within(subscription.next()).await.is_none());
}

#[tokio::test]
async fn second_subscribe_for_a_live_session_is_rejected_without_disturbing_the_first() {
    // Arrange
    let server = RawServer::bind();
    let (client, mut conn) = server.connect(timing()).await;
    let session = SessionId::new("s1");
    let (first, ()) = tokio::join!(client.subscribe(&session, None), conn.ack_subscribe());
    let mut first = first.unwrap();

    // Act
    let second = client.subscribe(&session, None).await;
    conn.event(&session, 1).await;

    // Assert
    assert!(
        matches!(second, Err(ClientError::AlreadySubscribed { .. })),
        "got {:?}",
        second.err()
    );
    let event = within(first.next()).await.unwrap();
    assert_eq!(event.params.unwrap()["seq"], 1);
}

#[tokio::test]
async fn rejected_subscribe_during_a_pending_one_keeps_the_pending_registration() {
    // Arrange
    let server = RawServer::bind();
    let (client, mut conn) = server.connect(timing()).await;
    let session = SessionId::new("s1");
    let first = client.subscribe(&session, None);

    // Act
    let server_side = async {
        let (id, _, _) = conn.recv_request().await;
        // The first subscribe is now registered and waiting for its ack.
        let second = client.subscribe(&session, None).await;
        conn.reply(&id, json!({"subscribed": true})).await;
        conn.event(&session, 1).await;
        second
    };
    let (first, second) = tokio::join!(first, server_side);

    // Assert
    assert!(matches!(second, Err(ClientError::AlreadySubscribed { .. })));
    let mut first = first.unwrap();
    let event = within(first.next()).await.unwrap();
    assert_eq!(event.params.unwrap()["seq"], 1);
}

#[tokio::test]
async fn failed_subscribe_releases_the_session_for_a_later_one() {
    // Arrange
    let server = RawServer::bind();
    let (client, mut conn) = server.connect(timing()).await;
    let session = SessionId::new("s1");
    let rejecting = async {
        let (id, _, _) = conn.recv_request().await;
        conn.send(json!({"jsonrpc": "2.0", "id": id,
            "error": {"code": -32000, "message": "unknown session"}}))
            .await;
    };
    let (failed, ()) = tokio::join!(client.subscribe(&session, None), rejecting);
    assert!(matches!(failed, Err(ClientError::Rpc { .. })));

    // Act
    let (retry, ()) = tokio::join!(client.subscribe(&session, None), conn.ack_subscribe());

    // Assert
    assert!(retry.is_ok());
}

#[tokio::test]
async fn frame_of_maximum_size_followed_by_another_in_one_write_is_delivered() {
    // Arrange
    let server = RawServer::bind();
    let (client, mut conn) = server.connect(timing()).await;
    let session = SessionId::new("s1");
    let (subscription, ()) = tokio::join!(client.subscribe(&session, None), conn.ack_subscribe());
    let mut subscription = subscription.unwrap();
    let event = |seq: u64, body: String| {
        json!({"jsonrpc": "2.0", "method": "session.event",
            "params": {"session_id": session, "seq": seq, "kind": "k", "body": body}})
        .to_string()
    };
    let overhead = event(1, String::new()).len();
    let big = event(1, "x".repeat(MAX_FRAME_BYTES - overhead));
    assert_eq!(big.len(), MAX_FRAME_BYTES);
    let mut bytes = encode_frame(big.as_bytes()).unwrap();
    for seq in 2..=4 {
        bytes.extend(encode_frame(event(seq, "b".into()).as_bytes()).unwrap());
    }

    // Act
    conn.stream.write_all(&bytes).await.unwrap();

    // Assert
    for expected in 1..=4 {
        let n = within(subscription.next()).await.expect("stream ended");
        assert_eq!(n.params.unwrap()["seq"], expected);
    }
}

#[tokio::test]
async fn unread_subscription_ends_and_the_connection_keeps_working() {
    // Arrange
    // Matches the client's per subscription queue capacity of 256.
    const QUEUE_CAPACITY: u64 = 256;
    let server = RawServer::bind();
    let (client, mut conn) = server.connect(timing()).await;
    let session = SessionId::new("s1");
    let (subscription, ()) = tokio::join!(client.subscribe(&session, None), conn.ack_subscribe());
    let mut subscription = subscription.unwrap();

    // Act
    for seq in 1..=QUEUE_CAPACITY + 1 {
        conn.event(&session, seq).await;
    }
    let server_side = async {
        let (id, method, _) = conn.recv_request().await;
        assert_eq!(method, "ping");
        conn.reply(&id, json!({"pong": true})).await;
    };
    let empty = json!({});
    let (pong, ()) = tokio::join!(client.request::<_, Value>("ping", &empty), server_side);

    // Assert
    let mut delivered = 0;
    while within(subscription.next()).await.is_some() {
        delivered += 1;
    }
    assert_eq!(delivered, QUEUE_CAPACITY);
    assert_eq!(pong.unwrap(), json!({"pong": true}));
}

#[tokio::test]
async fn late_response_to_a_timed_out_request_is_ignored() {
    // Arrange
    let server = RawServer::bind();
    let short = Timing {
        idle_timeout: Duration::from_millis(100),
        ..Timing::for_tests()
    };
    let (client, mut conn) = server.connect(short).await;
    let first: Result<Value, _> = client.request("slow", &json!({})).await;
    let (stale_id, _, _) = conn.recv_request().await;

    // Act
    conn.reply(&stale_id, json!({"stale": true})).await;
    let server_side = async {
        let (id, method, _) = conn.recv_request().await;
        conn.reply(&id, json!({ "answer": method })).await;
    };
    let empty = json!({});
    let (second, ()) = tokio::join!(client.request::<_, Value>("next", &empty), server_side);

    // Assert
    assert!(matches!(first, Err(ClientError::Timeout { .. })));
    assert_eq!(second.unwrap(), json!({"answer": "next"}));
}

#[tokio::test]
async fn resume_does_not_redeliver_a_permission_request_already_seen() {
    // Arrange
    let server = RawServer::bind();
    let (client, mut conn) = server.connect(timing()).await;
    let session = SessionId::new("s1");
    let (subscription, ()) = tokio::join!(client.subscribe(&session, None), conn.ack_subscribe());
    let mut subscription = subscription.unwrap();
    conn.permission(&session, "r1").await;
    within(subscription.next()).await.unwrap();
    let (second_client, mut second_conn) = server.connect(timing()).await;

    // Act
    let (resumed, ()) = tokio::join!(
        second_client.resume(&subscription),
        second_conn.ack_subscribe()
    );
    let mut resumed = resumed.unwrap();
    second_conn.permission(&session, "r1").await;
    second_conn.permission(&session, "r2").await;

    // Assert
    let n = within(resumed.next()).await.unwrap();
    assert_eq!(n.params.unwrap()["request_id"], "r2");
}
