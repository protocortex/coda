// SPDX-License-Identifier: Apache-2.0

//! `session.compact`: compacts a Claude Code session's conversation between turns.

use coda_config::config::Config;
use coda_protocol::jsonrpc::INVALID_PARAMS;
use coda_protocol::types::BackendKind;
use serde_json::{Value, json};

use super::{Rig, SESSION_NOT_FOUND, TURN_RUNNING, relaxed};

const COMPACT_LOG: &str = "compact.log";
const BACKEND_SESSION: &str = "fake-session-1";

async fn compact(rig: &mut Rig) -> Value {
    let params = json!({"session_id": rig.session});
    rig.call("session.compact", params).await
}

async fn finish_a_turn(rig: &mut Rig) {
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_until_idle().await;
}

fn log_path(rig: &Rig) -> std::path::PathBuf {
    rig.cwd.path().join(COMPACT_LOG)
}

#[tokio::test]
async fn a_session_with_a_conversation_is_compacted_by_a_resume_of_its_backend_id() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.control(&json!({"compact_log": log_path(&rig)}));
    finish_a_turn(&mut rig).await;

    // Act
    let answer = compact(&mut rig).await;

    // Assert
    assert_eq!(answer["result"], json!({}), "{answer}");
    let seen = std::fs::read_to_string(log_path(&rig)).unwrap();
    assert!(seen.contains("/compact"), "{seen}");
    assert!(
        seen.contains(&format!("--resume {BACKEND_SESSION}")),
        "{seen}"
    );
    assert!(!seen.contains("--disable-slash-commands"), "{seen}");
}

#[tokio::test]
async fn a_send_a_stop_and_a_close_during_a_compaction_find_the_session_busy() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    let gate = rig.cwd.path().join("compact.gate");
    rig.control(&json!({"compact_gate": gate, "compact_log": log_path(&rig)}));
    finish_a_turn(&mut rig).await;
    let params = json!({"session_id": rig.session});
    let first = rig.request(100, "session.compact", params);
    tokio::pin!(first);

    // Act
    let (compaction, (send, stop, close)) = tokio::join!(&mut first, async {
        while !log_path(&rig).exists() {
            tokio::time::sleep(super::POLL).await;
        }
        let send = rig
            .request(
                101,
                "session.send",
                json!({"session_id": rig.session, "text": "x"}),
            )
            .await;
        let stop = rig
            .request(102, "session.cancel", json!({"session_id": rig.session}))
            .await;
        let close = rig
            .request(103, "session.close", json!({"session_id": rig.session}))
            .await;
        std::fs::write(&gate, b"open").unwrap();
        (send, stop, close)
    });

    // Assert
    assert_eq!(send["error"]["code"], TURN_RUNNING, "{send}");
    assert_eq!(stop["result"]["cancelled"], false, "{stop}");
    assert_eq!(close["error"]["code"], TURN_RUNNING, "{close}");
    assert_eq!(compaction["result"], json!({}), "{compaction}");
}

#[tokio::test]
async fn a_session_that_has_not_talked_yet_has_nothing_to_compact() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let answer = compact(&mut rig).await;

    // Assert
    assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no conversation"),
        "{answer}"
    );
}

#[tokio::test]
async fn a_running_turn_refuses_the_compaction() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.send("hello").await;
    rig.wait_for_pending().await;

    // Act
    let answer = compact(&mut rig).await;

    // Assert
    assert_eq!(answer["error"]["code"], TURN_RUNNING, "{answer}");
}

#[tokio::test]
async fn a_failed_compaction_says_why_and_leaves_the_session_usable() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.control(&json!({"compact_error": "Not enough messages to compact."}));
    finish_a_turn(&mut rig).await;

    // Act
    let answer = compact(&mut rig).await;
    let again = rig.send("still works").await;

    // Assert
    assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Not enough messages"),
        "{answer}"
    );
    assert!(again["result"]["turn_id"].is_string(), "{again}");
}

#[tokio::test]
async fn only_a_claude_code_session_can_be_compacted() {
    // Arrange
    let mut rig = Rig::build(
        relaxed(),
        Vec::new(),
        BackendKind::CodexAppServer,
        Config::default(),
    );

    // Act
    let answer = compact(&mut rig).await;

    // Assert
    assert_eq!(answer["error"]["code"], INVALID_PARAMS, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Claude Code"),
        "{answer}"
    );
}

#[tokio::test]
async fn an_unknown_session_is_not_found() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    let answer = rig
        .call("session.compact", json!({"session_id": "no-such-session"}))
        .await;

    // Assert
    assert_eq!(answer["error"]["code"], SESSION_NOT_FOUND, "{answer}");
}
