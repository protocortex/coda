// SPDX-License-Identifier: Apache-2.0

//! The `todo` tool in a model-API session: each change is stored as an event,
//! and a list survives the loop being rebuilt, as after a daemon restart.

use coda_protocol::events::{TODO_UPDATED_KIND, TodoStatus, TodoUpdatedBody};
use coda_protocol::types::Mode;
use serde_json::{Value, json};

use super::{Rig, text_turn, tool_turn};

fn todo_turn(id: &str, arguments: Value) -> Value {
    tool_turn(&[(id, "todo", arguments)])
}

fn tool_texts(rig: &Rig) -> Vec<String> {
    let lines = rig.request_lines();
    lines
        .last()
        .into_iter()
        .flat_map(|request| request["messages"].as_array().unwrap().clone())
        .filter(|message| message["role"] == "tool")
        .map(|message| message["text"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_change_is_stored_with_the_whole_list_and_the_list_outlives_a_rebuilt_loop() {
    // Arrange
    // A rebuilt session plays its script again from the start, so the second
    // send adds the same item once more.
    let script = [
        todo_turn("c1", json!({"action": "add", "text": "write tests"})),
        text_turn("planned"),
    ];
    let mut rig = Rig::new(Mode::Default, &script);
    rig.send("plan it").await;
    rig.wait_for_results(1).await;
    rig.wait_until_idle().await;
    rig.turns.models.loops.clear();

    // Act
    rig.send("again").await;
    rig.wait_for_results(2).await;
    rig.wait_until_idle().await;

    // Assert
    let stored = rig.bodies(TODO_UPDATED_KIND).await;
    assert_eq!(stored.len(), 2, "{stored:?}");
    let last: TodoUpdatedBody = serde_json::from_value(stored[1].clone()).unwrap();
    let texts: Vec<_> = last.items.iter().map(|item| item.text.as_str()).collect();
    assert_eq!(texts, ["write tests", "write tests"]);
    assert_eq!(last.items[1].id, 2);
    assert!(
        last.items
            .iter()
            .all(|item| item.status == TodoStatus::Pending)
    );
    assert_eq!(
        tool_texts(&rig),
        [
            "1. [ ] write tests",
            "1. [ ] write tests\n2. [ ] write tests"
        ]
    );
}
