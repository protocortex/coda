// SPDX-License-Identifier: Apache-2.0

//! The tool server client against a scripted server process.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use coda_testkit::paths::fake_tool_server_path;
use coda_testkit::wait::poll_until;
use coda_tools::tool_server::{
    ExternalTool, MAX_ARGUMENT_BYTES, MAX_TOOLS_PER_SERVER, ServerError, ServerSpec, Started,
    ToolServer, UNTRUSTED_LABEL,
};
use coda_tools::{DEFAULT_MAX_OUTPUT_BYTES, Tool, ToolError};
use rustix::process::{Pid, test_kill_process};
use serde_json::json;
use tempfile::TempDir;

const SERVER: &str = "srv";
const SHORT: Duration = Duration::from_millis(400);
const LONG: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(20);
const BOUND: Duration = Duration::from_secs(10);

fn spec(mode: &str, extra: &[&str], timeout: Duration, env: &[(&str, &str)]) -> ServerSpec {
    let mut argv = vec![
        fake_tool_server_path().unwrap().display().to_string(),
        mode.to_owned(),
    ];
    argv.extend(extra.iter().map(|arg| (*arg).to_owned()));
    ServerSpec {
        name: SERVER.to_owned(),
        argv,
        env: env
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect(),
        cwd: std::env::temp_dir(),
        timeout,
    }
}

fn start(mode: &str) -> Started {
    ToolServer::start(&spec(mode, &[], LONG, &[]), &[]).unwrap()
}

fn tool(started: &Started, raw: &str) -> ExternalTool {
    let def = started
        .tools
        .iter()
        .find(|def| def.raw_name == raw)
        .unwrap_or_else(|| panic!("no tool {raw}"));
    ExternalTool::new(Arc::clone(&started.server), def)
}

fn gone(pid: i32) -> bool {
    let pid = Pid::from_raw(pid).unwrap();
    poll_until(BOUND, POLL, || test_kill_process(pid).is_err())
}

fn pid_in(file: &PathBuf) -> i32 {
    assert!(poll_until(BOUND, POLL, || file.exists()), "no pid file");
    std::fs::read_to_string(file)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn a_servers_tools_are_listed_under_its_name_and_called() {
    // Arrange
    let started = start("default");

    // Act
    let names: Vec<&str> = started.tools.iter().map(|def| def.name.as_str()).collect();
    let out = tool(&started, "echo").call(&json!({"text": "hi"})).unwrap();

    // Assert
    assert!(
        names.contains(&"srv__echo") && names.contains(&"srv__show_env"),
        "{names:?}"
    );
    assert_eq!(
        out.text,
        format!("[srv__echo: {UNTRUSTED_LABEL}]\necho: hi")
    );
}

#[test]
fn the_server_sees_only_the_variables_it_was_given() {
    // Arrange
    let bare = ToolServer::start(&spec("default", &[], LONG, &[]), &[]).unwrap();
    let given = ToolServer::start(&spec("default", &[], LONG, &[("LISTED", "1")]), &[]).unwrap();

    // Act
    let none = tool(&bare, "show_env").call(&json!({})).unwrap();
    let one = tool(&given, "show_env").call(&json!({})).unwrap();

    // Assert
    assert!(none.text.ends_with("]\n"), "{:?}", none.text);
    assert!(one.text.ends_with("]\nLISTED"), "{:?}", one.text);
}

#[test]
fn a_server_that_crashes_on_a_call_is_reported_and_stays_stopped() {
    // Arrange
    let started = start("default");

    // Act
    let first = tool(&started, "boom").call(&json!({})).unwrap_err();
    let second = tool(&started, "echo")
        .call(&json!({"text": "again"}))
        .unwrap_err();

    // Assert
    assert!(matches!(first, ToolError::External { .. }), "{first}");
    assert!(first.to_string().contains("the server stopped"), "{first}");
    assert!(
        second.to_string().contains("the server stopped"),
        "{second}"
    );
    assert!(started.server.is_stopped());
}

#[test]
fn a_server_that_hangs_is_killed_with_its_process_group_and_reported() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("pid");
    let started = ToolServer::start(
        &spec("default", &[pid_file.to_str().unwrap()], SHORT, &[]),
        &[],
    )
    .unwrap();

    // Act
    let error = tool(&started, "hang").call(&json!({})).unwrap_err();

    // Assert
    assert!(error.to_string().contains("did not answer"), "{error}");
    assert!(error.to_string().contains("killed"), "{error}");
    assert!(gone(pid_in(&pid_file)), "the grandchild survived");
    assert!(started.server.is_stopped());
}

#[test]
fn a_cancelled_call_kills_the_server() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("pid");
    let started = ToolServer::start(
        &spec("default", &[pid_file.to_str().unwrap()], LONG, &[]),
        &[],
    )
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let waiting = tool(&started, "hang");
    let call = std::thread::spawn(move || waiting.call_cancellable(&json!({}), &flag));

    // Act
    let pid = pid_in(&pid_file);
    cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    let error = call.join().unwrap().unwrap_err();

    // Assert
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert!(gone(pid), "the grandchild survived");
}

#[test]
fn dropping_a_server_kills_its_process_group() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let pid_file = dir.path().join("pid");
    let started = ToolServer::start(
        &spec("default", &[pid_file.to_str().unwrap()], SHORT, &[]),
        &[],
    )
    .unwrap();
    let hang = tool(&started, "hang");
    let _ = hang.call(&json!({}));
    let pid = pid_in(&pid_file);

    // Act
    drop(hang);
    drop(started);

    // Assert
    assert!(gone(pid), "the grandchild survived");
}

#[test]
fn tools_that_fail_the_checks_are_refused_with_a_reason_and_the_rest_are_kept() {
    // Arrange / Act
    let started = ToolServer::start(&spec("odd-tools", &[], LONG, &[]), &["bash"]).unwrap();

    // Assert
    let kept: Vec<&str> = started
        .tools
        .iter()
        .map(|def| def.raw_name.as_str())
        .collect();
    assert_eq!(kept, ["good", "long_text"]);
    let reasons: Vec<(&str, &str)> = started
        .refused
        .iter()
        .map(|r| (r.name.as_str(), r.reason.as_str()))
        .collect();
    assert!(
        reasons
            .iter()
            .any(|(n, r)| *n == "bash" && r.contains("clashes with a built-in"))
    );
    assert!(
        reasons
            .iter()
            .any(|(n, r)| *n == "bad name" && r.contains("name of 1 to 64"))
    );
    assert!(
        reasons
            .iter()
            .any(|(n, r)| *n == "not_object" && r.contains("not an object schema"))
    );
    assert!(
        reasons
            .iter()
            .any(|(n, r)| *n == "huge_schema" && r.contains("over 8 KiB"))
    );
    assert!(
        reasons
            .iter()
            .any(|(n, r)| *n == "good" && r.contains("already taken"))
    );
    let long = started
        .tools
        .iter()
        .find(|d| d.raw_name == "long_text")
        .unwrap();
    assert!(long.description.len() <= 2048);
}

#[test]
fn a_server_that_stops_reading_cannot_hold_a_call_past_its_time_limit() {
    // Arrange
    let started = ToolServer::start(&spec("deaf", &[], SHORT, &[]), &[]).unwrap();
    let big = json!({"text": "x".repeat(MAX_ARGUMENT_BYTES - 20)});

    // Act
    let error = tool(&started, "echo").call(&big).unwrap_err();

    // Assert
    assert!(error.to_string().contains("did not answer"), "{error}");
    assert!(started.server.is_stopped());
}

#[test]
fn a_server_with_too_many_tools_keeps_the_first_ones() {
    // Act
    let started = start("many-tools");

    // Assert
    assert_eq!(started.tools.len(), MAX_TOOLS_PER_SERVER);
    assert_eq!(started.refused.len(), 40 - MAX_TOOLS_PER_SERVER);
}

#[test]
fn a_server_that_never_answers_the_handshake_or_exits_at_once_fails_to_start() {
    // Act
    let silent = ToolServer::start(&spec("silent", &[], SHORT, &[]), &[]);
    let crashed = ToolServer::start(&spec("startup-crash", &[], LONG, &[]), &[]);
    let missing = ToolServer::start(
        &ServerSpec {
            argv: vec!["/no/such/program".to_owned()],
            ..spec("default", &[], LONG, &[])
        },
        &[],
    );

    // Assert
    assert!(matches!(silent, Err(ServerError::TimedOut(_))));
    assert!(matches!(crashed, Err(ServerError::Exited)));
    assert!(matches!(missing, Err(ServerError::Spawn(_))));
}

#[test]
fn a_server_that_sends_a_huge_line_or_text_that_is_not_json_is_killed() {
    // Act
    let big = ToolServer::start(&spec("bigline", &[], LONG, &[]), &[]);
    let bad = ToolServer::start(&spec("badjson", &[], LONG, &[]), &[]);

    // Assert
    assert!(matches!(big, Err(ServerError::Protocol(_))));
    assert!(matches!(bad, Err(ServerError::Protocol(_))));
}

#[test]
fn a_request_the_server_makes_is_refused_and_a_notification_is_passed_over() {
    // Arrange
    let started = start("chatty");

    // Act
    let out = tool(&started, "echo").call(&json!({"text": "ok"})).unwrap();

    // Assert
    assert!(out.text.ends_with("echo: ok"), "{:?}", out.text);
}

#[test]
fn an_error_result_is_reported_and_leaves_the_server_running() {
    // Arrange
    let started = start("default");

    // Act
    let error = tool(&started, "fail").call(&json!({})).unwrap_err();
    let later = tool(&started, "echo").call(&json!({"text": "still"}));

    // Assert
    assert!(error.to_string().contains("nope"), "{error}");
    assert!(later.is_ok());
}

#[test]
fn a_long_reply_is_cut_to_the_cap_and_a_non_text_part_is_noted() {
    // Arrange
    let started = start("default");

    // Act
    let big = tool(&started, "bigout").call(&json!({})).unwrap();
    let other = tool(&started, "nontext").call(&json!({})).unwrap();

    // Assert
    assert_eq!(big.text.len(), DEFAULT_MAX_OUTPUT_BYTES);
    assert!(big.truncated);
    assert!(
        other.text.ends_with("[non-text content omitted]"),
        "{:?}",
        other.text
    );
}

#[test]
fn arguments_that_are_not_an_object_or_are_too_big_are_not_sent() {
    // Arrange
    let started = start("default");
    let echo = tool(&started, "echo");
    let big = json!({"text": "x".repeat(MAX_ARGUMENT_BYTES)});

    // Act
    let array = echo.call(&json!([1])).unwrap_err();
    let huge = echo.call(&big).unwrap_err();

    // Assert
    assert!(matches!(array, ToolError::InvalidInput { .. }));
    assert!(matches!(huge, ToolError::InvalidInput { .. }));
    assert!(!started.server.is_stopped());
}
