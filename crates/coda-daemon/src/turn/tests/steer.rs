// SPDX-License-Identifier: Apache-2.0

//! `session.send` with `mode: steer`.

use coda_protocol::jsonrpc::INVALID_PARAMS;
use serde_json::{Value, json};

use super::{CODEX_REPORT, Rig, codex_rig, relaxed};

async fn steer(rig: &mut Rig, text: &str) -> Value {
    let params = json!({"session_id": rig.session, "text": text, "mode": "steer"});
    rig.call("session.send", params).await
}

async fn steered_rows(rig: &Rig) -> Vec<Value> {
    rig.events()
        .await
        .into_iter()
        .filter(|e| e.kind == "user_message")
        .map(|e| serde_json::from_str::<Value>(&e.body).unwrap())
        .filter(|body| body["steered"] == true)
        .collect()
}

#[tokio::test]
async fn a_steer_with_no_turn_running_is_refused() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let answer = steer(&mut rig, "use tabs").await;

    // Assert
    assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
}

#[tokio::test]
async fn a_steer_reaches_the_claude_child_and_is_stored_in_the_turn() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let prompts = rig.cwd.path().join("prompts.txt");
    rig.control(&json!({ "prompt_path": prompts }));
    let sent = rig.send("tool-slow").await;

    // Act
    let answer = steer(&mut rig, "use tabs").await;
    rig.wait_for("result").await;

    // Assert
    assert_eq!(answer["result"]["steered"], true, "{answer}");
    assert_eq!(answer["result"]["turn_id"], sent["result"]["turn_id"]);
    assert_eq!(steered_rows(&rig).await[0]["text"], "use tabs");
    rig.wait_until_idle().await;
    let seen = std::fs::read_to_string(&prompts).unwrap();
    assert!(seen.contains("use tabs"), "{seen}");
}

#[tokio::test]
async fn a_steer_reaches_the_codex_server_as_turn_steer() {
    // Arrange
    let mut rig = codex_rig(&json!({}));
    rig.send("stall").await;
    rig.wait_for("user_message").await;

    // Act
    let answer = steer(&mut rig, "use tabs").await;
    rig.wait_for("result").await;
    rig.wait_until_turn_over().await;

    // Assert
    assert_eq!(answer["result"]["steered"], true, "{answer}");
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(rig.cwd.path().join(CODEX_REPORT)).unwrap())
            .unwrap();
    assert_eq!(report["steer_params"]["input"][0]["text"], "use tabs");
}
