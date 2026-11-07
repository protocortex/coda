// SPDX-License-Identifier: Apache-2.0

//! A `client_request_id` makes `session.create`, `session.send` and
//! `session.cancel` answer once.

use coda_protocol::jsonrpc::INVALID_PARAMS;
use serde_json::{Value, json};

use super::{Rig, relaxed};

const REQUEST: &str = "req-1";

async fn send_with(rig: &mut Rig, text: &str, request: &str, extra: Value) -> Value {
    let mut params = json!({
        "session_id": rig.session,
        "text": text,
        "client_request_id": request,
    });
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    rig.call("session.send", params).await
}

async fn user_messages(rig: &Rig) -> usize {
    rig.events()
        .await
        .iter()
        .filter(|e| e.kind == "user_message")
        .count()
}

async fn create(rig: &mut Rig, request: &str) -> Value {
    let params = json!({
        "backend": "claude_code",
        "cwd": rig.cwd.path(),
        "client_request_id": request,
    });
    rig.call("session.create", params).await
}

async fn session_count(rig: &Rig) -> usize {
    rig.hub
        .with_store(|store| {
            store
                .list_sessions(&coda_store::SessionFilter::default())
                .map(|sessions| sessions.len())
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn a_repeated_send_while_the_turn_runs_returns_the_first_answer() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let first = send_with(&mut rig, "hello", REQUEST, json!({})).await;

    // Act
    let again = send_with(&mut rig, "hello", REQUEST, json!({})).await;

    // Assert
    assert!(first["result"]["turn_id"].is_string(), "{first}");
    assert_eq!(again["result"], first["result"]);
    assert_eq!(user_messages(&rig).await, 1);
}

#[tokio::test]
async fn a_repeated_send_after_the_turn_ended_does_not_start_another() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let first = send_with(&mut rig, "hello", REQUEST, json!({})).await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;

    // Act
    let again = send_with(&mut rig, "hello", REQUEST, json!({})).await;

    // Assert
    assert_eq!(again["result"], first["result"]);
    assert_eq!(user_messages(&rig).await, 1);
}

#[tokio::test]
async fn a_send_without_a_request_id_is_not_deduplicated() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;

    // Act
    let second = rig.send("hello").await;

    // Assert
    assert_eq!(second["error"]["code"], super::TURN_RUNNING, "{second}");
}

#[tokio::test]
async fn an_id_used_for_another_session_is_refused() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    send_with(&mut rig, "hello", REQUEST, json!({})).await;
    let other = rig.other_session().await;

    // Act
    let params = json!({"session_id": other, "text": "hi", "client_request_id": REQUEST});
    let answer = rig.call("session.send", params).await;

    // Assert
    assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
}

#[tokio::test]
async fn an_id_used_for_another_method_is_refused() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    send_with(&mut rig, "hello", REQUEST, json!({})).await;

    // Act
    let params = json!({"session_id": rig.session, "client_request_id": REQUEST});
    let answer = rig.call("session.cancel", params).await;

    // Assert
    assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
}

#[tokio::test]
async fn an_empty_or_oversized_id_is_refused() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let long = "x".repeat(coda_protocol::methods::MAX_REQUEST_ID_BYTES + 1);

    // Act
    let empty = send_with(&mut rig, "hello", "", json!({})).await;
    let oversized = send_with(&mut rig, "hello", &long, json!({})).await;

    // Assert
    assert_eq!(empty["error"]["code"], INVALID_PARAMS, "{empty}");
    assert_eq!(oversized["error"]["code"], INVALID_PARAMS, "{oversized}");
}

#[tokio::test]
async fn a_repeated_create_returns_the_same_session() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let before = session_count(&rig).await;
    let first = create(&mut rig, REQUEST).await;

    // Act
    let again = create(&mut rig, REQUEST).await;

    // Assert
    assert!(first["result"]["session"]["id"].is_string(), "{first}");
    assert_eq!(again["result"], first["result"]);
    assert_eq!(session_count(&rig).await, before + 1);
}

#[tokio::test]
async fn a_repeated_cancel_returns_the_first_answer_and_cancels_nothing_new() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    rig.wait_for_pending().await;
    let params = json!({"session_id": rig.session, "client_request_id": REQUEST});
    let first = rig.call("session.cancel", params.clone()).await;
    rig.wait_for("turn_interrupted").await;
    rig.wait_until_turn_over().await;
    rig.send("again").await;
    rig.wait_for_pending().await;

    // Act
    let repeat = rig.call("session.cancel", params).await;

    // Assert
    assert_eq!(first["result"]["cancelled"], true);
    assert_eq!(repeat["result"], first["result"]);
    assert!(rig.turns.is_running(&rig.session));
}

#[tokio::test]
async fn two_queued_sends_with_one_id_at_once_queue_one_message() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("first").await;
    rig.wait_for_pending().await;
    let params = json!({
        "session_id": rig.session,
        "text": "second",
        "mode": "queue",
        "client_request_id": REQUEST,
    });

    // Act
    let (a, b) = tokio::join!(
        crate::turn::send(Some(&rig.turns), Some(params.clone())),
        crate::turn::send(Some(&rig.turns), Some(params)),
    );

    // Assert
    assert_eq!(a.unwrap(), b.unwrap());
    let list = rig
        .call(
            "session.queue",
            json!({"session_id": rig.session, "action": "list"}),
        )
        .await;
    assert_eq!(list["result"]["messages"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_repeated_queued_send_queues_one_message() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("first").await;
    rig.wait_for_pending().await;
    let extra = json!({"mode": "queue"});
    let first = send_with(&mut rig, "second", REQUEST, extra.clone()).await;

    // Act
    let again = send_with(&mut rig, "second", REQUEST, extra).await;

    // Assert
    assert_eq!(first["result"]["queued"], true, "{first}");
    assert_eq!(again["result"], first["result"]);
    let list = rig
        .call(
            "session.queue",
            json!({"session_id": rig.session, "action": "list"}),
        )
        .await;
    assert_eq!(list["result"]["messages"].as_array().unwrap().len(), 1);
}
