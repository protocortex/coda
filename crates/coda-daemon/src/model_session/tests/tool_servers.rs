// SPDX-License-Identifier: Apache-2.0

//! External tool servers in a running session: only the user's own config adds
//! one, and every call asks the user first.

use std::path::Path;

use coda_config::config::Config;
use coda_config::tool_servers::{DEFAULT_SERVER_TIMEOUT_MS, ToolServerConfig};
use coda_protocol::types::Mode;
use coda_testkit::paths::fake_tool_server_path;
use coda_tools::tool_server::{UNTRUSTED_LABEL, UNTRUSTED_RULE};
use serde_json::json;

use super::skills::write;
use super::{Rig, text_turn, tool_turn};

const SERVER: &str = "srv";
const CALL: &str = "srv__echo";

fn list_fake_server(config: &mut Config) {
    config.tool_servers.insert(
        SERVER.to_owned(),
        ToolServerConfig {
            command: vec![
                fake_tool_server_path().unwrap().display().to_string(),
                "default".to_owned(),
            ],
            env: Vec::new(),
            timeout_ms: DEFAULT_SERVER_TIMEOUT_MS,
        },
    );
}

fn echo_turn() -> serde_json::Value {
    tool_turn(&[("c1", CALL, json!({"text": "hi"}))])
}

async fn offered_with(tune: impl FnOnce(&mut Config)) -> (Vec<String>, String) {
    let mut rig = Rig::custom(Mode::Default, &[text_turn("hi")], true, None, tune);
    rig.send("hello").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;
    let first = rig.request_lines().remove(0);
    let tools = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool.as_str().unwrap().to_owned())
        .collect();
    (tools, first["system"].as_str().unwrap().to_owned())
}

#[tokio::test]
async fn a_session_offers_a_servers_tools_and_the_untrusted_rule_only_when_the_user_listed_it() {
    // Act
    let (off_tools, off_system) = offered_with(|_| {}).await;
    let (on_tools, on_system) = offered_with(list_fake_server).await;

    // Assert
    assert!(
        !off_tools.iter().any(|name| name.contains("__")),
        "{off_tools:?}"
    );
    assert!(!off_system.contains(UNTRUSTED_RULE));
    assert!(on_tools.contains(&CALL.to_owned()), "{on_tools:?}");
    assert!(on_system.contains(UNTRUSTED_RULE), "{on_system}");
}

#[tokio::test]
async fn a_tool_server_call_waits_for_the_user_and_runs_only_after_an_allow() {
    // Arrange
    let mut rig = Rig::custom(
        Mode::Default,
        &[echo_turn(), text_turn("done")],
        true,
        None,
        list_fake_server,
    );
    rig.send("echo").await;

    // Act
    let request = rig.wait_for_pending().await;
    let before = rig.bodies("tool_finished").await.len();
    rig.respond(&request, "allow_once").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(before, 0);
    let finished = rig.bodies("tool_finished").await;
    assert_eq!(finished[0]["is_error"], false);
    let sent = rig.request_lines().last().unwrap().to_string();
    assert!(sent.contains(UNTRUSTED_LABEL), "{sent}");
    assert!(sent.contains("echo: hi"), "{sent}");
}

#[tokio::test]
async fn a_denied_tool_server_call_never_reaches_the_server() {
    // Arrange
    let mut rig = Rig::custom(
        Mode::Default,
        &[echo_turn(), text_turn("done")],
        true,
        None,
        list_fake_server,
    );
    rig.send("echo").await;
    let request = rig.wait_for_pending().await;

    // Act
    rig.respond(&request, "deny").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(rig.bodies("tool_finished").await[0]["is_error"], true);
    assert!(
        !rig.request_lines()
            .last()
            .unwrap()
            .to_string()
            .contains("echo: hi")
    );
}

fn project_files(cwd: &Path, marker: &Path) {
    let command = format!("{:?}", ["/usr/bin/touch", &marker.display().to_string()]);
    write(
        cwd,
        ".coda/config.toml",
        &format!("[tool_servers.evil]\ncommand = {command}\n"),
    );
    let mcp = json!({"mcpServers": {"evil": {
        "command": "/usr/bin/touch", "args": [marker.display().to_string()],
    }}});
    write(cwd, ".mcp.json", &mcp.to_string());
}

#[tokio::test]
async fn a_server_named_in_a_project_file_is_never_started_or_offered() {
    // Arrange
    let marker_dir = tempfile::tempdir().unwrap();
    let marker = marker_dir.path().join("started");
    let mut rig = Rig::custom(
        Mode::Default,
        &[text_turn("hi")],
        true,
        None,
        list_fake_server,
    );
    project_files(&rig.cwd, &marker);

    // Act
    rig.send("hello").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let first = rig.request_lines().remove(0);
    assert!(first.to_string().contains(CALL), "{first}");
    assert!(!first.to_string().contains("evil__"), "{first}");
    assert!(!marker.exists(), "the project file started a program");
}
