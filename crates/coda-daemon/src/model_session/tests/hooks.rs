// SPDX-License-Identifier: AGPL-3.0-only

//! Hooks in a model session: the environment they get and a blocking hook that
//! stops a tool call.

use std::path::Path;

use coda_config::hooks::{HookConfig, HookEvent};
use coda_protocol::events::{HOOK_BLOCKED_KIND, HookBlockedBody};
use coda_protocol::types::Mode;
use serde_json::{Value, json};

use coda_core::hooks::MAX_RESULT_BYTES;

use super::switch::faux_alias;
use super::{API_KEY_CANARY, POLL, Rig, WAIT, text_turn, tool_turn};
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
    let blocked = rig.bodies(HOOK_BLOCKED_KIND).await;
    let body: HookBlockedBody = serde_json::from_value(blocked[0].clone()).unwrap();
    assert!(body.reason.contains("not on this network"), "{body:?}");
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

/// Logs the payload to `$1`, one line each, then fails unless `$1.allow` exists.
const GATE: &str =
    r#"cat >> "$1"; echo >> "$1"; test -e "$1.allow" || { echo "not yet" >&2; exit 9; }"#;

/// Sends `text` and waits for the `ended`th turn to end.
async fn send_and_finish(rig: &mut Rig, text: &str, ended: usize) {
    rig.send(text).await;
    rig.wait_for_results(ended).await;
    rig.wait_until_idle().await;
}

async fn switch_model(rig: &mut Rig) {
    let params = json!({"session_id": rig.session, "model": "scripted"});
    let response = rig.call("session.set_model", params).await;
    assert!(response["error"].is_null(), "{response}");
}

fn resumed_flags(log: &Path) -> Vec<Value> {
    payloads(log).iter().map(|p| p["resumed"].clone()).collect()
}

#[tokio::test]
async fn a_session_start_hook_is_told_the_session_is_resumed_once_it_passed_in_an_earlier_loop() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let log = out.path().join("starts.jsonl");
    std::fs::write(out.path().join("starts.jsonl.allow"), "").unwrap();
    let hook = shell_hook(HookEvent::SessionStart, GATE, &log, true);
    let script = [text_turn("one"), text_turn("two")];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, |config| {
        faux_alias(config);
        config.hooks.push(hook);
    });

    // Act
    send_and_finish(&mut rig, "first", 1).await;
    switch_model(&mut rig).await;
    send_and_finish(&mut rig, "second", 2).await;

    // Assert
    assert_eq!(resumed_flags(&log), [json!(false), json!(true)]);
    assert_eq!(rig.bodies("session_started").await.len(), 1);
}

#[tokio::test]
async fn a_blocked_first_send_is_still_a_first_start_after_the_loop_is_rebuilt() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let log = out.path().join("starts.jsonl");
    let hook = shell_hook(HookEvent::SessionStart, GATE, &log, true);
    let script = [text_turn("one"), text_turn("two")];
    let mut rig = Rig::custom(Mode::Default, &script, true, None, |config| {
        faux_alias(config);
        config.hooks.push(hook);
    });

    // Act
    send_and_finish(&mut rig, "blocked", 1).await;
    let started_when_blocked = rig.bodies("session_started").await.len();
    switch_model(&mut rig).await;
    std::fs::write(out.path().join("starts.jsonl.allow"), "").unwrap();
    send_and_finish(&mut rig, "retry", 2).await;
    switch_model(&mut rig).await;
    send_and_finish(&mut rig, "again", 3).await;

    // Assert
    // The history held the blocked send's messages when the retry's loop was
    // built, and the retry was still the session's first start.
    assert_eq!(
        resumed_flags(&log),
        [json!(false), json!(false), json!(true)]
    );
    assert_eq!(started_when_blocked, 0);
    assert_eq!(rig.bodies("session_started").await.len(), 1);
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

/// Waits for the file a hook writes when it starts.
async fn wait_for_file(file: &Path) {
    tokio::time::timeout(WAIT, async {
        while !file.exists() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{} was not written in time", file.display()));
}

/// A line of the big file the model-visible result drops.
const SECRET_LINE: &str = "line 500 of the big file";

#[tokio::test]
async fn cancelling_a_sub_agents_post_tool_hook_leaves_nothing_to_retrieve_of_a_compacted_result() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let up = out.path().join("up");
    let script = r#"cat > "$1/seen"; echo up > "$1/up"; sleep 30"#;
    let hook = shell_hook(HookEvent::PostTool, script, out.path(), true);
    let mut turns = scout_script();
    turns[1] = tool_turn(&[("r1", "read", json!({"path": "big.txt"}))]);
    let mut rig = Rig::custom(Mode::Default, &turns, true, None, |config| {
        config.hooks.push(hook);
    });
    install_scout(&rig);
    // Many lines, so reading it is compacted and the original is kept. The
    // line sits in the middle, where compaction drops it from the shrunk text.
    let big = (0..1000)
        .map(|n| format!("line {n} of the big file\n"))
        .collect::<Vec<_>>()
        .concat();
    assert!(big.contains(SECRET_LINE));
    std::fs::write(rig.cwd.join("big.txt"), big).unwrap();
    rig.send("go").await;
    wait_for_file(&up).await;
    let told = std::fs::read_to_string(out.path().join("seen")).unwrap();
    let (_, tail) = told.rsplit_once("retrieve id ").expect(&told);
    let id: String = tail.chars().take_while(char::is_ascii_digit).collect();
    let before = rig.original(&id).await;

    // Act
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let kept_before = before.unwrap_or_default();
    assert!(
        kept_before.contains(SECRET_LINE),
        "control: original {id} held the file"
    );
    // A deleted id is handed out again, so it may now hold the withheld note.
    let after = rig.original(&id).await.unwrap_or_default();
    assert!(
        !after.contains(SECRET_LINE),
        "retrieve {id} returns the file: {after}"
    );
    let stored = super::dump(&rig.db(), "originals");
    assert!(
        !stored.contains(SECRET_LINE),
        "an original still holds the file"
    );
}

#[tokio::test]
async fn cancelling_a_sub_agents_post_tool_hook_withholds_the_childs_result() {
    // Arrange
    let out = tempfile::tempdir().unwrap();
    let started = out.path().join("started");
    let hook = shell_hook(
        HookEvent::PostTool,
        r#"echo up > "$1"; sleep 30"#,
        &started,
        true,
    );
    let mut rig = Rig::custom(Mode::Default, &scout_script(), true, None, |config| {
        config.hooks.push(hook);
    });
    install_scout(&rig);
    rig.send("go").await;
    wait_for_file(&started).await;

    // Act
    rig.call("session.cancel", json!({"session_id": rig.session}))
        .await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let children = rig.bodies("child_event").await;
    let finished = |e: &&Value| e["kind"] == "tool_finished" && e["body"]["call_id"] == "r1";
    let read = children
        .iter()
        .find(finished)
        .expect("the scout's read ended");
    assert_eq!(read["body"]["is_error"], true);
    let original = read["body"]["original"].as_str().unwrap();
    let told = rig.original(original).await.unwrap();
    assert!(told.contains("withheld"), "{told}");
    assert!(!told.contains("one needle here"), "{told}");
    assert_eq!(
        rig.request_lines().len(),
        2,
        "no model call after the cancel"
    );
}
