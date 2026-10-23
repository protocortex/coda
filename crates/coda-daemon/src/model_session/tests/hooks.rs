// SPDX-License-Identifier: AGPL-3.0-only

//! Hooks in a model session: the environment they get and a blocking hook that
//! stops a tool call.

use std::path::Path;

use coda_config::hooks::{HookConfig, HookEvent};
use coda_protocol::types::Mode;
use serde_json::{Value, json};

use coda_core::hooks::MAX_RESULT_BYTES;

use super::{API_KEY_CANARY, Rig, text_turn, tool_turn};
use crate::model_session::COMPACT_THRESHOLD_BYTES;

/// Far longer than a hook needs: the first run of a program can be slow on a busy machine.
const PATIENT_MS: u64 = 20_000;

/// A hook that runs `script` with `argument` as `$1`.
fn shell_hook(event: HookEvent, script: &str, argument: &Path, blocking: bool) -> HookConfig {
    HookConfig {
        event,
        command: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            script.to_owned(),
            "hook".to_owned(),
            argument.to_string_lossy().into_owned(),
        ],
        blocking,
        timeout_ms: Some(PATIENT_MS),
    }
}

#[tokio::test]
async fn a_hook_in_a_model_session_gets_the_allowlisted_environment_and_no_api_key() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let dump = out.path().join("env.txt");
    let hook = shell_hook(HookEvent::SessionStart, r#"env > "$1""#, &dump, false);
    let mut rig = Rig::custom(Mode::Default, &[text_turn("hi")], true, None, |config| {
        config.hooks.push(hook);
    });

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let env = std::fs::read_to_string(&dump).expect("the session_start hook did not run");
    assert!(env.lines().any(|line| line.starts_with("PATH=")), "{env}");
    assert!(!env.contains("ANTHROPIC_API_KEY"), "{env}");
    assert!(!env.contains(API_KEY_CANARY), "{env}");
}

#[tokio::test]
async fn a_blocking_session_start_hook_stops_the_turn_and_its_reason_is_not_model_text() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let hook = shell_hook(
        HookEvent::SessionStart,
        r#"echo "not on this network" >&2; exit 9"#,
        out.path(),
        true,
    );
    let mut rig = Rig::custom(Mode::Default, &[text_turn("hi")], true, None, |config| {
        config.hooks.push(hook);
    });

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let blocked = rig.bodies("hook_blocked").await;
    let reason = blocked[0]["reason"].as_str().unwrap();
    assert!(reason.contains("not on this network"), "{reason}");
    let stopped = rig.bodies("turn_stopped").await;
    assert_eq!(stopped[0]["condition"], "hook_blocked");
    assert_eq!(rig.bodies("stream_delta").await.len(), 0);
    assert!(rig.requests().is_empty(), "the model must not be called");
}

#[tokio::test]
async fn a_blocking_pre_tool_hook_stops_the_tool_and_the_model_sees_why() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let hook = shell_hook(
        HookEvent::PreTool,
        r#"echo "no reads on friday" >&2; exit 3"#,
        out.path(),
        true,
    );
    let read = tool_turn(&[("c1", "read", json!({"path": "a.txt"}))]);
    let mut rig = Rig::custom(
        Mode::Default,
        &[read, text_turn("understood")],
        true,
        None,
        |config| config.hooks.push(hook),
    );

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let finished = rig.bodies("tool_finished").await;
    assert_eq!(finished[0]["is_error"], true, "{finished:?}");
    let requests = rig.requests();
    let second = serde_json::to_string(&requests[1]).unwrap();
    assert!(second.contains("blocked by the pre_tool hook"), "{second}");
    assert!(second.contains("no reads on friday"), "{second}");
    assert!(!second.contains("one needle here"), "{second}");
}

/// A user agent that can read and search; the faux script plays its turns.
const SCOUT: &str = "+++\nname = \"scout\"\n\
    description = \"Looks around. Use when asked. Do not use otherwise.\"\n\
    tier = \"fast\"\neffort = \"low\"\ntools = [\"read\", \"grep\"]\n+++\nYou are a scout.\n";

/// The turns of a session that sends `scout` out to read a file: the session's
/// `task` call, the scout's `read` call, its report, the session's last words.
fn scout_script() -> [Value; 4] {
    let task = json!({"agent": "scout", "prompt": "look around"});
    [
        tool_turn(&[("t1", "task", task)]),
        tool_turn(&[("r1", "read", json!({"path": "a.txt"}))]),
        text_turn("the scout is done"),
        text_turn("parent done"),
    ]
}

fn install_scout(rig: &Rig) {
    let agents = rig.home().join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("scout.md"), SCOUT).unwrap();
}

#[tokio::test]
async fn a_blocking_pre_tool_hook_stops_a_sub_agents_tool_call_too() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let seen = out.path().join("seen.txt");
    let script = r#"cat >> "$1"; echo "no reads on friday" >&2; exit 3"#;
    let hook = shell_hook(HookEvent::PreTool, script, &seen, true);
    let mut rig = Rig::custom(Mode::Default, &scout_script(), true, None, |config| {
        config.hooks.push(hook);
    });
    install_scout(&rig);

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 4, "the scout, then the session, ask the model");
    let scout_next = lines[2]["messages"].to_string();
    assert!(
        scout_next.contains("blocked by the pre_tool hook"),
        "{scout_next}"
    );
    assert!(scout_next.contains("no reads on friday"), "{scout_next}");
    assert!(!scout_next.contains("one needle here"), "{scout_next}");
    // The hook saw the scout's call and not the session's `task` call, which is
    // not an action on the machine.
    let payloads = std::fs::read_to_string(&seen).unwrap();
    assert_eq!(
        payloads.matches("\"event\":\"pre_tool\"").count(),
        1,
        "{payloads}"
    );
    assert!(payloads.contains("\"tool\":\"read\""), "{payloads}");
}

#[tokio::test]
async fn a_sub_agent_does_not_run_the_session_start_hooks() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let starts = out.path().join("starts.txt");
    let hook = shell_hook(
        HookEvent::SessionStart,
        r#"echo start >> "$1""#,
        &starts,
        false,
    );
    let mut rig = Rig::custom(Mode::Default, &scout_script(), true, None, |config| {
        config.hooks.push(hook);
    });
    install_scout(&rig);

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    assert_eq!(rig.request_lines().len(), 4);
    let ran = std::fs::read_to_string(&starts).unwrap();
    assert_eq!(ran, "start\n", "only the session starts, not its sub-agent");
}

/// The payloads a hook logged to `file`, one JSON object per line.
fn payloads(file: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let lines = text.lines().filter(|line| !line.is_empty());
    lines
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// The compaction footer names the original's size and id.
const FOOTER_ROOM_BYTES: usize = 256;

// A hook gets every result the daemon hands the model, unless it is huge.
const _: () = assert!(MAX_RESULT_BYTES >= COMPACT_THRESHOLD_BYTES + FOOTER_ROOM_BYTES);

#[tokio::test]
async fn a_post_tool_hook_sees_the_whole_result_the_model_sees() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let seen = out.path().join("seen.jsonl");
    let script = r#"cat >> "$1"; echo >> "$1""#;
    let hook = shell_hook(HookEvent::PostTool, script, &seen, true);
    let read = tool_turn(&[("c1", "read", json!({"path": "big.txt"}))]);
    let mut rig = Rig::custom(
        Mode::Default,
        &[read, text_turn("understood")],
        true,
        None,
        |config| config.hooks.push(hook),
    );
    // Past the 4 KiB the hook used to get, and below what the daemon compacts.
    let secret_at_the_end = "SECRET-AT-THE-END";
    let body = format!("{}\n{secret_at_the_end}\n", "x".repeat(10 * 1024));
    std::fs::write(rig.cwd.join("big.txt"), body).unwrap();

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let payloads = payloads(&seen);
    assert_eq!(payloads.len(), 1);
    let result = payloads[0]["result"].as_str().unwrap();
    assert!(
        result.contains(secret_at_the_end),
        "cut at {}",
        result.len()
    );
    assert!(payloads[0].get("result_truncated").is_none());
    let second = serde_json::to_string(&rig.requests()[1]).unwrap();
    assert!(second.contains(secret_at_the_end), "{second}");
}

#[tokio::test]
async fn a_blocking_post_tool_hook_guards_a_sub_agents_tool_output_too() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let seen = out.path().join("seen.txt");
    let script = r#"cat >> "$1"; echo "scout output vetoed" >&2; exit 3"#;
    let hook = shell_hook(HookEvent::PostTool, script, &seen, true);
    let mut rig = Rig::custom(Mode::Default, &scout_script(), true, None, |config| {
        config.hooks.push(hook);
    });
    install_scout(&rig);

    // Act
    rig.send("go").await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let lines = rig.request_lines();
    assert_eq!(lines.len(), 4, "the scout, then the session, ask the model");
    let scout_next = lines[2]["messages"].to_string();
    assert!(
        scout_next.contains("blocked by the post_tool hook"),
        "{scout_next}"
    );
    assert!(scout_next.contains("scout output vetoed"), "{scout_next}");
    assert!(!scout_next.contains("one needle here"), "{scout_next}");
    // The hook saw the scout's `read` and not the session's `task` call.
    let logged = std::fs::read_to_string(&seen).unwrap();
    assert_eq!(logged.matches("\"event\":\"post_tool\"").count(), 1);
    assert!(logged.contains("\"tool\":\"read\""), "{logged}");
}
