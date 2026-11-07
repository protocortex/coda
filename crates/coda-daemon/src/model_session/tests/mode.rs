// SPDX-License-Identifier: Apache-2.0

//! `session.set_mode` on a model-API session drops its idle loop and keeps what it spent.

use coda_protocol::types::Mode;
use serde_json::json;

use super::{Rig, TOKEN_CAP, read_turn, text_turn};

#[tokio::test]
async fn a_mode_change_drops_the_idle_loop_and_the_next_send_rebuilds_the_history() {
    // Arrange
    let script = [text_turn("one"), text_turn("two")];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, |_| {});
    rig.send("first").await;
    rig.wait_for_results(1).await;
    rig.wait_until_idle().await;
    let kept_before = rig.turns.models.loops.len();

    // Act
    let params = json!({"session_id": rig.session, "mode": "plan"});
    let response = rig.call("session.set_mode", params).await;
    let kept_after = rig.turns.models.loops.len();
    rig.send("second").await;
    rig.wait_for_results(2).await;

    // Assert
    assert_eq!(response["result"]["session"]["mode"], "plan", "{response}");
    assert_eq!((kept_before, kept_after), (1, 0));
    assert_eq!(
        rig.requests()[1],
        [
            json!({"role": "user", "text": "first"}),
            json!({"role": "assistant", "text": "one", "tool_calls": []}),
            json!({"role": "user", "text": "second"}),
        ]
    );
}

#[tokio::test]
async fn a_mode_change_keeps_the_tokens_the_session_spent() {
    // Arrange
    // A read turn (8 tokens) and a reply (18) leave 4 of the 30 tokens; the
    // rebuilt loop starts the script again, so its read turn must hit the cap.
    let script = [read_turn(), text_turn("done")];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, |config| {
        config.token_cap = TOKEN_CAP;
    });
    rig.send("first").await;
    rig.wait_until_idle().await;
    let params = json!({"session_id": rig.session, "mode": "plan"});
    rig.call("session.set_mode", params).await;

    // Act
    rig.send("second").await;
    rig.wait_until_idle().await;

    // Assert
    let stops = rig.bodies("turn_stopped").await;
    let conditions: Vec<_> = stops.iter().map(|stop| &stop["condition"]).collect();
    assert_eq!(conditions, ["end_turn", "budget_exhausted"]);
}
