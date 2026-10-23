// SPDX-License-Identifier: AGPL-3.0-only

//! Skills in a running session: what the model is offered follows the user's
//! trust decision and the files at every send, not only at the first one.

use std::path::Path;

use coda_agents::{revoke_project, trust_project};
use coda_protocol::types::Mode;
use serde_json::{Value, json};

use super::task::agent_file;
use super::{Rig, TOKEN_CAP, assistant_message, read_turn, text_turn, tool_turn, user_message};
use crate::model_session::COMPACT_THRESHOLD_BYTES;

const PROJECT_BODY: &str = "PROJECT-BODY-MARKER lint before you ship";
const EDITED_BODY: &str = "EDITED-BODY-MARKER skip the lint";
const USER_BODY: &str = "USER-BODY-MARKER fill every field";

pub(super) fn skill_file(name: &str, description: &str, body: &str) -> String {
    format!("+++\nname = \"{name}\"\ndescription = {description:?}\n+++\n{body}\n")
}

pub(super) fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn skill_call(name: &str) -> Value {
    tool_turn(&[("c1", "skill", json!({"name": name}))])
}

/// Whether the model was offered the `skill` tool in `request`.
fn offers_skill(request: &Value) -> bool {
    request["tools"]
        .as_array()
        .unwrap()
        .contains(&json!("skill"))
}

/// The schema named `name` among the tools of a logged request.
pub(super) fn schema_of(request: &Value, name: &str) -> Value {
    let schemas = request["tool_schemas"].as_array().unwrap();
    let found = schemas.iter().find(|schema| schema["name"] == name);
    found
        .unwrap_or_else(|| panic!("no {name} tool in {schemas:?}"))
        .clone()
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
        // What the model already loaded stays in its history after the change.
        let replayed = requests[2]["messages"][2]["text"].as_str().unwrap();
        assert!(replayed.contains(PROJECT_BODY), "{change:?}: {replayed}");
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
async fn a_skill_body_over_the_compaction_threshold_reaches_the_next_request_whole() {
    // Arrange
    // Many lines, so compaction would keep a few of them, and between the
    // 16 KiB a tool result is compacted at and the 32 KiB skill cap.
    let steps = (0..500)
        .map(|n| format!("step {n}: do the careful thing and check it\n"))
        .collect::<Vec<_>>()
        .concat();
    let body = format!("{steps}END-OF-THE-SKILL");
    assert!(body.len() > COMPACT_THRESHOLD_BYTES);
    assert!(body.len() < coda_agents::MAX_SKILL_BODY_BYTES);
    let script = [skill_call("big"), text_turn("done")];
    let mut rig = Rig::new(Mode::Default, &script);
    write(
        &rig.home(),
        "skills/big.md",
        &skill_file("big", "A long one.", &body),
    );

    // Act
    rig.send("go").await;
    rig.wait_until_idle().await;

    // Assert
    let loaded = rig.requests()[1][2]["text"].as_str().unwrap().to_owned();
    assert!(
        loaded.contains(&body),
        "the model got {} bytes",
        loaded.len()
    );
    assert!(!loaded.contains("retrieve id"), "the body was compacted");
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

#[tokio::test]
async fn the_model_is_sent_the_skill_index_and_only_the_names_it_may_load() {
    // Arrange
    // A rebuilt loop starts the script again, so both sends use its first turn.
    let script = [text_turn("done")];
    let mut rig = trusted_project_skill(&script).await;
    rig.send("first").await;
    rig.wait_until_idle().await;
    let repo = rig.cwd.clone();
    rig.hub
        .with_store_mut(move |store| revoke_project(store, &repo))
        .await
        .unwrap();

    // Act
    rig.send("second").await;
    rig.wait_until_idle().await;

    // Assert
    let lines = rig.request_lines();
    let trusted = schema_of(&lines[0], "skill");
    let index = trusted["description"].as_str().unwrap();
    assert!(index.contains("- local: Lints the project.\n"), "{index}");
    assert!(index.contains("- pdf: Fills in PDF forms.\n"), "{index}");
    assert_eq!(
        trusted["input_schema"]["properties"]["name"]["enum"],
        json!(["local", "pdf"])
    );
    let revoked = schema_of(&lines[1], "skill");
    let index = revoked["description"].as_str().unwrap();
    assert!(!index.contains("local"), "{index}");
    assert!(index.contains("- pdf: Fills in PDF forms.\n"), "{index}");
    assert_eq!(
        revoked["input_schema"]["properties"]["name"]["enum"],
        json!(["pdf"])
    );
}

#[tokio::test]
async fn a_revoke_in_a_repository_with_agents_and_no_skills_rebuilds_the_loop_and_keeps_the_tokens()
{
    // Arrange
    // The project holds an agent and no skill, so a revoke changes only the
    // agents the `task` tool lists. A read turn and a reply leave 4 of the 30
    // tokens, and a rebuilt loop starts the script again with a read turn.
    let script = [read_turn(), text_turn("done")];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, |config| {
        config.token_cap = TOKEN_CAP;
    });
    let agent = agent_file("local", "\"read\"", "Local.\n");
    write(&rig.cwd, ".coda/agents/local.md", &agent);
    let repo = rig.cwd.clone();
    rig.hub
        .with_store_mut(move |store| trust_project(store, &repo))
        .await
        .unwrap();
    rig.send("first").await;
    rig.wait_until_idle().await;
    let repo = rig.cwd.clone();
    rig.hub
        .with_store_mut(move |store| revoke_project(store, &repo))
        .await
        .unwrap();

    // Act
    rig.send("second").await;
    rig.wait_until_idle().await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 3, "the second send makes one call");
    let listed = |request: &Value| {
        let task = schema_of(request, "task");
        task["description"].as_str().unwrap().to_owned()
    };
    assert!(listed(&lines[0]).contains("- local: "), "trusted");
    let after = listed(&lines[2]);
    assert!(!after.contains("- local: "), "{after}");
    assert!(after.contains("- explore: "), "the built-ins stay: {after}");
    let stops = rig.bodies("turn_stopped").await;
    let conditions: Vec<_> = stops.iter().map(|stop| &stop["condition"]).collect();
    assert_eq!(conditions, ["end_turn", "budget_exhausted"]);
}
