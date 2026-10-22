// SPDX-License-Identifier: AGPL-3.0-only

//! Skills in a running session: what the model is offered follows the user's
//! trust decision and the files at every send, not only at the first one.

use std::path::Path;

use coda_agents::{revoke_project, trust_project};
use coda_protocol::types::Mode;
use serde_json::{Value, json};

use super::{Rig, assistant_message, text_turn, tool_turn, user_message};

const TOKEN_CAP: u64 = 30;
const PROJECT_BODY: &str = "PROJECT-BODY-MARKER lint before you ship";
const EDITED_BODY: &str = "EDITED-BODY-MARKER skip the lint";
const USER_BODY: &str = "USER-BODY-MARKER fill every field";

fn skill_file(name: &str, description: &str, body: &str) -> String {
    format!("+++\nname = \"{name}\"\ndescription = {description:?}\n+++\n{body}\n")
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn skill_call(name: &str) -> Value {
    json!({"events": [
        {"type": "tool_call_start", "call_id": "c1", "name": "skill"},
        {"type": "tool_call_arguments_delta", "call_id": "c1",
         "fragment": json!({"name": name}).to_string()},
        {"type": "stop", "reason": "tool_use"},
    ]})
}

/// A model turn that reads a file, which needs no permission.
fn read_turn() -> Value {
    tool_turn(&[("c1", "read", json!({"path": "a.txt"}))])
}

/// Whether the model was offered the `skill` tool in `request`.
fn offers_skill(request: &Value) -> bool {
    request["tools"]
        .as_array()
        .unwrap()
        .contains(&json!("skill"))
}

/// A rig with a user skill and a project skill the user has trusted.
async fn trusted_project_skill(script: &[Value]) -> Rig {
    let rig = Rig::new(Mode::Default, script);
    let pdf = skill_file("pdf", "Fills in PDF forms.", USER_BODY);
    write(&rig.home(), "skills/pdf.md", &pdf);
    let local = skill_file("local", "Lints the project.", PROJECT_BODY);
    write(&rig.cwd, ".coda/skills/local.md", &local);
    let repo = rig.cwd.clone();
    rig.hub
        .with_store_mut(move |store| trust_project(store, &repo))
        .await
        .unwrap();
    rig
}

#[derive(Debug, Clone, Copy)]
enum Change {
    Revoke,
    Edit,
}

#[tokio::test]
async fn a_revoked_or_edited_project_skill_leaves_a_running_session_at_the_next_send() {
    for change in [Change::Revoke, Change::Edit] {
        // Arrange
        // A rebuilt loop starts the script again, so the model asks for the
        // project skill in both sends.
        let script = [skill_call("local"), text_turn("done")];
        let mut rig = trusted_project_skill(&script).await;
        rig.send("first").await;
        rig.wait_until_idle().await;

        // Act
        match change {
            Change::Revoke => {
                let repo = rig.cwd.clone();
                rig.hub
                    .with_store_mut(move |store| revoke_project(store, &repo))
                    .await
                    .unwrap();
            }
            Change::Edit => write(
                &rig.cwd,
                ".coda/skills/local.md",
                &skill_file("local", "Lints the project.", EDITED_BODY),
            ),
        }
        rig.send("second").await;
        rig.wait_until_idle().await;

        // Assert
        let requests = rig.request_lines();
        assert_eq!(requests.len(), 4, "{change:?}");
        assert!(requests.iter().all(offers_skill), "{change:?}");
        let before = requests[1]["messages"][2]["text"].as_str().unwrap();
        assert!(before.contains(PROJECT_BODY), "{change:?}: {before}");
        let refused = requests[3]["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(refused["is_error"], true, "{change:?}: {refused}");
        let sent = requests[2..]
            .iter()
            .map(Value::to_string)
            .collect::<String>();
        assert!(!sent.contains("EDITED-BODY-MARKER"), "{change:?}");
    }
}

#[tokio::test]
async fn unchanged_skills_keep_the_running_conversation() {
    // Arrange
    let script = [skill_call("local"), text_turn("done"), text_turn("again")];
    let mut rig = trusted_project_skill(&script).await;
    rig.send("first").await;
    rig.wait_until_idle().await;
    let before = rig.turns.models.loops.holds(&rig.session);

    // Act
    rig.send("second").await;
    rig.wait_until_idle().await;

    // Assert
    assert!(before, "the idle loop should be kept between sends");
    let requests = rig.request_lines();
    // A kept loop goes on with the script; a rebuilt one would start it again.
    assert_eq!(requests.len(), 3);
    assert!(offers_skill(&requests[2]));
}

#[tokio::test]
async fn a_rebuild_after_a_skill_change_keeps_the_tokens_the_session_spent() {
    // Arrange
    // A read turn (8 tokens) and a reply (18) leave 4 of the 30 tokens; a rebuilt
    // loop starts the script again, so its read turn must hit the original cap.
    let script = [read_turn(), text_turn("done")];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, |config| {
        config.token_cap = TOKEN_CAP;
    });
    write(
        &rig.home(),
        "skills/pdf.md",
        &skill_file("pdf", "Fills in PDF forms.", USER_BODY),
    );
    rig.send("first").await;
    rig.wait_until_idle().await;
    let extra = skill_file("extra", "Does extra work.", USER_BODY);
    write(&rig.home(), "skills/extra.md", &extra);

    // Act
    rig.send("second").await;
    rig.wait_until_idle().await;

    // Assert
    let stops = rig.bodies("turn_stopped").await;
    let conditions: Vec<_> = stops.iter().map(|stop| &stop["condition"]).collect();
    assert_eq!(conditions, ["end_turn", "budget_exhausted"]);
    assert_eq!(
        rig.request_lines().len(),
        3,
        "the second send makes one call"
    );
}

#[tokio::test]
async fn a_skill_call_replays_from_the_store_when_the_conversation_is_rebuilt() {
    // Arrange
    let script = [skill_call("pdf"), text_turn("done")];
    let mut rig = Rig::new(Mode::Default, &script);
    let pdf = skill_file("pdf", "Fills in PDF forms.", USER_BODY);
    write(&rig.home(), "skills/pdf.md", &pdf);
    rig.send("first").await;
    rig.wait_until_idle().await;
    // What a daemon restart leaves: no loop in memory, the events in the store.
    rig.turns.models.loops.clear();

    // Act
    rig.send("second").await;
    rig.wait_until_idle().await;

    // Assert
    let requests = rig.requests();
    let call = json!([{"id": "c1", "name": "skill", "arguments": "{\"name\":\"pdf\"}"}]);
    let loaded = requests[2][2]["text"].as_str().unwrap();
    assert!(loaded.contains(USER_BODY), "{loaded}");
    assert_eq!(requests[2][0], user_message("first"));
    assert_eq!(requests[2][1], assistant_message("", &call));
    assert_eq!(requests[2][2]["call_id"], "c1");
    assert_eq!(requests[2][2]["is_error"], false);
    assert_eq!(requests[2][3], assistant_message("done", &json!([])));
    assert_eq!(requests[2][4], user_message("second"));
}

#[tokio::test]
async fn a_sub_agent_is_not_offered_the_skill_tool() {
    // Arrange
    // One script feeds the parent, then the child, then the parent again.
    let task = json!({"agent": "scout", "prompt": "look around"});
    let script = [
        tool_turn(&[("t1", "task", task)]),
        text_turn("nothing here"),
        text_turn("parent done"),
    ];
    let mut rig = Rig::new(Mode::Default, &script);
    let pdf = skill_file("pdf", "Fills in PDF forms.", USER_BODY);
    write(&rig.home(), "skills/pdf.md", &pdf);
    let scout = "+++\nname = \"scout\"\n\
        description = \"Looks around. Use when asked. Do not use otherwise.\"\n\
        tier = \"fast\"\neffort = \"low\"\ntools = [\"read\", \"grep\"]\n+++\nYou are a scout.\n";
    write(&rig.home(), "agents/scout.md", scout);

    // Act
    rig.send("please search").await;
    rig.wait_for_results(1).await;
    rig.wait_until_idle().await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 3);
    assert!(offers_skill(&lines[0]), "the session itself has the tool");
    assert_eq!(lines[1]["system"], "You are a scout.\n");
    assert_eq!(lines[1]["tools"], json!(["grep", "read"]));
}
