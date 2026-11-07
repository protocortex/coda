// SPDX-License-Identifier: Apache-2.0

//! The `ask` tool in a model-API session: the question reaches the client as a
//! permission request with options, and the answer comes back as the option.

use coda_protocol::types::Mode;
use serde_json::{Value, json};

use super::{Rig, text_turn, tool_turn};

fn ask_turn() -> Value {
    tool_turn(&[(
        "c1",
        "ask",
        json!({"question": "Which file?", "options": ["a.rs", "b.rs"]}),
    )])
}

fn last_tool_text(rig: &Rig) -> String {
    let lines = rig.request_lines();
    let messages = lines.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone();
    messages
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .map(|message| message["text"].as_str().unwrap().to_owned())
        .unwrap()
}

#[tokio::test]
async fn the_option_the_client_picks_comes_back_to_the_model() {
    // Arrange
    let mut rig = Rig::new(Mode::Default, &[ask_turn(), text_turn("done")]);

    // Act
    rig.send("go").await;
    let request = rig.wait_for_pending().await;
    let pending = rig.pending().await;
    let params = json!({
        "session_id": rig.session, "request_id": request,
        "decision": "allow_once", "choice": 1,
    });
    rig.call("permission.respond", params).await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(pending[0].tool, "ask");
    assert_eq!(last_tool_text(&rig), "The user chose option 2: b.rs");
}

#[tokio::test]
async fn a_client_with_no_terminal_gets_the_model_an_error() {
    // Arrange
    let mut rig = Rig::new(Mode::Default, &[ask_turn(), text_turn("done")]);

    // Act
    rig.send("go").await;
    let request = rig.wait_for_pending().await;
    let params = json!({
        "session_id": rig.session, "request_id": request,
        "decision": "deny", "no_terminal": true,
    });
    rig.call("permission.respond", params).await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(last_tool_text(&rig).contains("no one is at a terminal"));
    assert_eq!(rig.bodies("tool_finished").await[0]["is_error"], true);
}
