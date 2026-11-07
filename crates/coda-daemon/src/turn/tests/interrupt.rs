// SPDX-License-Identifier: Apache-2.0

//! A cancelled Claude Code turn is interrupted, and its process takes the next turn.

use serde_json::json;

use super::{Rig, relaxed};

const START_LOG: &str = "starts.txt";

fn starts(rig: &Rig) -> usize {
    std::fs::read_to_string(rig.cwd.path().join(START_LOG)).map_or(0, |raw| raw.lines().count())
}

#[tokio::test]
async fn the_claude_process_of_a_cancelled_turn_serves_the_next_turn() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());
    rig.control(&json!({"start_log": rig.cwd.path().join(START_LOG)}));
    rig.send("interruptible").await;
    rig.wait_for("tool_call").await;

    // Act
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_for("turn_interrupted").await;
    rig.wait_until_turn_over().await;
    let kept = rig.pidfile().exists();
    rig.send("hello").await;
    let request = rig.wait_for_pending().await;
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_turn_over().await;

    // Assert
    assert!(kept, "the process was stopped after the interrupt");
    assert_eq!(starts(&rig), 1);
}
