// SPDX-License-Identifier: Apache-2.0

//! `session.send` with `mode: queue` and `session.queue`.

use coda_store::MAX_QUEUED_MESSAGES;
use serde_json::{Value, json};

use super::{POLL, Rig, TURN_RUNNING, WAIT, relaxed};

/// Long enough for a start that a hold should have prevented to show itself.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

async fn queue_send(rig: &mut Rig, text: &str) -> Value {
    let params = json!({"session_id": rig.session, "text": text, "mode": "queue"});
    rig.call("session.send", params).await
}

async fn queue_call(rig: &mut Rig, extra: Value) -> Value {
    let mut params = json!({"session_id": rig.session});
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    rig.call("session.queue", params).await
}

async fn user_messages(rig: &Rig) -> Vec<String> {
    rig.events()
        .await
        .into_iter()
        .filter(|e| e.kind == "user_message")
        .map(|e| {
            serde_json::from_str::<Value>(&e.body).unwrap()["text"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

async fn wait_for_user_messages(rig: &Rig, count: usize) {
    tokio::time::timeout(WAIT, async {
        while user_messages(rig).await.len() < count {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("the queued message did not start in time");
}

#[tokio::test]
async fn a_queued_send_on_a_free_session_starts_at_once() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let sent = queue_send(&mut rig, "hello").await;

    // Assert
    assert!(sent["result"]["turn_id"].is_string(), "{sent}");
    assert!(sent["result"].get("queued").is_none(), "{sent}");
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn a_message_queued_during_a_turn_starts_when_the_turn_ends() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("first").await;
    let request = rig.wait_for_pending().await;

    // Act
    let queued = queue_send(&mut rig, "second").await;
    rig.respond(&request, "allow_once").await;
    wait_for_user_messages(&rig, 2).await;

    // Assert
    assert_eq!(queued["result"]["queued"], true, "{queued}");
    assert_eq!(user_messages(&rig).await, ["first", "second"]);
    let second = rig.wait_for_pending().await;
    rig.respond(&second, "allow_once").await;
    rig.wait_until_idle().await;
    let left = queue_call(&mut rig, json!({"action": "list"})).await;
    assert_eq!(left["result"]["messages"], json!([]));
}

#[tokio::test]
async fn a_plain_send_during_a_turn_is_still_refused() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("first").await;

    // Act
    let second = rig.send("again").await;

    // Assert
    assert_eq!(second["error"]["code"], TURN_RUNNING);
}

#[tokio::test]
async fn a_cancelled_turn_holds_the_queue_until_it_is_resumed() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("first").await;
    rig.wait_for_pending().await;
    queue_send(&mut rig, "second").await;

    // Act
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_for("turn_interrupted").await;
    rig.wait_until_idle().await;
    tokio::time::sleep(SETTLE).await;
    let held = queue_call(&mut rig, json!({"action": "list"})).await;
    let started_while_held = user_messages(&rig).await;
    let resumed = queue_call(&mut rig, json!({"action": "resume"})).await;

    // Assert
    assert_eq!(held["result"]["messages"][0]["text"], "second");
    assert_eq!(started_while_held, ["first"]);
    assert!(resumed["result"]["started"].is_string(), "{resumed}");
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn a_discarded_message_never_starts() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("first").await;
    let request = rig.wait_for_pending().await;
    queue_send(&mut rig, "second").await;
    let listed = queue_call(&mut rig, json!({"action": "list"})).await;
    let id = listed["result"]["messages"][0]["id"].clone();

    // Act
    let after = queue_call(&mut rig, json!({"action": "discard", "queue_id": id})).await;
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(after["result"]["messages"], json!([]));
    assert_eq!(user_messages(&rig).await, ["first"]);
}

#[tokio::test]
async fn a_full_queue_refuses_another_message() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("first").await;
    rig.wait_for_pending().await;
    for _ in 0..MAX_QUEUED_MESSAGES {
        queue_send(&mut rig, "x").await;
    }

    // Act
    let refused = queue_send(&mut rig, "one too many").await;

    // Assert
    assert_eq!(
        refused["error"]["code"],
        coda_protocol::jsonrpc::INVALID_PARAMS,
        "{refused}"
    );
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_until_idle().await;
}

#[tokio::test]
async fn a_plain_send_to_a_held_session_releases_the_queue_behind_it() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("first").await;
    rig.wait_for_pending().await;
    queue_send(&mut rig, "second").await;
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_for("turn_interrupted").await;
    rig.wait_until_idle().await;

    // Act
    rig.send("third").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    wait_for_user_messages(&rig, 3).await;

    // Assert
    assert_eq!(user_messages(&rig).await, ["first", "third", "second"]);
    let last = rig.wait_for_pending().await;
    rig.respond(&last, "allow_once").await;
    rig.wait_until_idle().await;
}
