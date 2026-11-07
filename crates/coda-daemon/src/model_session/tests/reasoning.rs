// SPDX-License-Identifier: Apache-2.0

//! Reasoning in a model session: kept out of the event log, sent back on the
//! next request.

use coda_protocol::types::Mode;
use serde_json::json;

use super::{Rig, text_turn};

const THOUGHT: &str = "the file is probably a.txt";

#[tokio::test]
async fn reasoning_is_sent_back_with_the_next_request_and_kept_out_of_the_event_log() {
    // Arrange
    let thinking = json!({"type": "reasoning", "block": {
        "type": "thinking", "text": THOUGHT, "signature": "sig-1"}});
    let call = json!({"events": [
        thinking,
        {"type": "tool_call_start", "call_id": "c1", "name": "read"},
        {"type": "tool_call_arguments_delta", "call_id": "c1", "fragment": "{\"path\": \"a.txt\"}"},
        {"type": "stop", "reason": "tool_use"},
    ]});
    let mut rig = Rig::custom(
        Mode::Default,
        &[call, text_turn("done")],
        true,
        None,
        |_| {},
    );

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let requests = rig.requests();
    let second = &requests[1];
    let reasoning = second
        .iter()
        .find(|message| message["role"] == "reasoning")
        .expect("the second request carries the reasoning");
    assert_eq!(reasoning["blocks"][0]["text"], THOUGHT);
    assert_eq!(reasoning["blocks"][0]["signature"], "sig-1");
    let stored: Vec<_> = rig.events().await.into_iter().map(|e| e.body).collect();
    assert!(
        stored.iter().all(|body| !body.contains(THOUGHT)),
        "{stored:?}"
    );
    let assistants = rig.bodies("assistant_message").await;
    assert!(assistants[0]["reasoning"].is_string(), "{assistants:?}");
    assert!(assistants[1].get("reasoning").is_none(), "{assistants:?}");
}
