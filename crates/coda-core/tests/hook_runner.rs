// SPDX-License-Identifier: AGPL-3.0-only

//! The hook runner on its own: what a hook is given, what stops it, what it
//! cannot see and what it leaves behind. Every hook is a small script in a
//! temporary directory or a `/bin/sh -c` line.

mod common;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use coda_config::hooks::{HookConfig, HookEvent};
use coda_core::hooks::{HookRunner, MAX_ARGUMENTS_BYTES, MAX_REASON_BYTES, MAX_RESULT_BYTES};
use coda_protocol::types::SessionId;
use common::{
    BOUND, STARTED_MS, Scratch, capture_logs, hook, json_file, pid_from, poll_until, process_alive,
    shell, timed,
};
use serde_json::json;

const TOOL: &str = "canned";

struct World {
    scratch: Scratch,
    cwd: PathBuf,
    session: SessionId,
}

impl World {
    fn new() -> Self {
        let scratch = Scratch::new();
        let cwd = scratch.out("ws");
        std::fs::create_dir(&cwd).unwrap();
        Self {
            scratch,
            cwd,
            session: SessionId::new("s-1"),
        }
    }

    fn out(&self, name: &str) -> PathBuf {
        self.scratch.out(name)
    }

    fn runner(hooks: &[HookConfig]) -> HookRunner {
        let env = vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))];
        HookRunner::new(hooks, env)
    }
}

/// A `cat` hook that keeps what it is given in `file`.
fn capture(event: HookEvent, file: &Path) -> HookConfig {
    shell(event, &format!("cat > {}", file.display()), false)
}

#[tokio::test]
async fn a_failing_blocking_hook_blocks_with_its_stderr_tail_as_the_reason() {
    // Arrange
    let w = World::new();
    let body = "echo 'first line' >&2\necho 'no deploys on friday' >&2\nexit 3";
    let script = w.scratch.script("deny.sh", body);
    let runner = World::runner(&[hook(HookEvent::PreTool, &[&script], true)]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    assert_eq!(blocked.event, HookEvent::PreTool);
    assert_eq!(blocked.hook, script);
    assert!(blocked.reason.contains("no deploys on friday"), "{blocked}");
    assert!(blocked.to_string().contains("pre_tool"), "{blocked}");
}

#[tokio::test]
async fn a_chatty_hook_keeps_only_the_end_of_standard_error_cut_on_a_character_boundary() {
    // Arrange
    let w = World::new();
    // 90 KB of three-byte characters, far past what the runner holds and more than
    // a pipe holds, so the cut falls inside a character.
    let line = "\u{20ac}".repeat(10);
    let script = format!(
        "printf HEAD-MARKER >&2\ni=0\nwhile [ $i -lt 3000 ]; do printf '{line}' >&2; i=$((i+1)); done\nprintf END >&2\nexit 1"
    );
    let runner = World::runner(&[shell(HookEvent::PreTool, &script, true)]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    let reason = &blocked.reason;
    // Whole characters only, so at most two bytes of the window are dropped.
    assert!(reason.len() <= MAX_REASON_BYTES, "{}", reason.len());
    assert!(reason.len() >= MAX_REASON_BYTES - 2, "{}", reason.len());
    assert!(!reason.contains("HEAD-MARKER"), "the start was kept");
    assert!(!reason.contains('\u{fffd}'), "the cut split a character");
    let body = reason.strip_suffix("END").expect("the end was kept");
    assert!(body.chars().all(|c| c == '\u{20ac}'), "{body}");
}

#[tokio::test]
async fn a_blocking_hook_with_an_empty_command_blocks_instead_of_panicking() {
    // Arrange
    let w = World::new();
    let runner = World::runner(&[hook(HookEvent::PreTool, &[], true)]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    assert_eq!(blocked.event, HookEvent::PreTool);
    assert!(blocked.reason.contains("empty command"), "{blocked}");
}

#[tokio::test]
async fn a_non_blocking_hook_with_an_empty_command_is_logged_and_the_call_goes_on() {
    // Arrange
    let (logs, _guard) = capture_logs();
    let w = World::new();
    let runner = World::runner(&[hook(HookEvent::PostTool, &[], false)]);

    // Act
    let outcome = runner
        .post_tool(&w.session, &w.cwd, TOOL, false, "ran")
        .await;

    // Assert
    assert!(outcome.is_ok());
    let logged = logs.text();
    assert!(
        logged.contains("hook failed, continuing: event=post_tool"),
        "{logged}"
    );
    assert!(logged.contains("empty command"), "{logged}");
}

#[tokio::test]
async fn a_blocking_hook_that_times_out_blocks_and_its_process_group_dies() {
    // Arrange
    let w = World::new();
    let pid_file = w.out("hook.pid");
    let script = format!("sleep 30 &\necho $! > {}\nwait", pid_file.display());
    let slow = timed(shell(HookEvent::PreTool, &script, true), STARTED_MS);
    let runner = World::runner(&[slow]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    let expected = format!("timed out after {STARTED_MS} ms");
    assert!(blocked.reason.contains(&expected), "{blocked}");
    let sleeper = pid_from(&pid_file).await;
    poll_until(|| !process_alive(&sleeper)).await;
}

#[tokio::test]
async fn a_failing_non_blocking_hook_logs_a_message_of_a_fixed_format_and_does_not_block() {
    // Arrange
    let (logs, _guard) = capture_logs();
    let w = World::new();
    let script = w
        .scratch
        .script("flaky.sh", "echo 'disk is full' >&2\nexit 7");
    let runner = World::runner(&[hook(HookEvent::PostTool, &[&script], false)]);

    // Act
    let outcome = runner
        .post_tool(&w.session, &w.cwd, TOOL, false, "ran")
        .await;

    // Assert
    assert!(outcome.is_ok());
    let expected = format!(
        "hook failed, continuing: event=post_tool hook={script} reason=exited with status 7: disk is full"
    );
    let logged = logs.text();
    assert!(logged.contains(&expected), "{logged}");
}

#[tokio::test]
async fn a_hook_gets_the_event_and_the_call_as_json_on_standard_input() {
    // Arrange
    let w = World::new();
    let (start, pre, post) = (w.out("start.json"), w.out("pre.json"), w.out("post.json"));
    let runner = World::runner(&[
        capture(HookEvent::SessionStart, &start),
        capture(HookEvent::PreTool, &pre),
        capture(HookEvent::PostTool, &post),
    ]);

    // Act
    runner
        .session_start(&w.session, &w.cwd, true)
        .await
        .unwrap();
    let args = json!({"path": "a.txt"});
    runner
        .pre_tool(&w.session, &w.cwd, TOOL, &args)
        .await
        .unwrap();
    runner
        .post_tool(&w.session, &w.cwd, TOOL, false, "the answer")
        .await
        .unwrap();

    // Assert
    let base =
        |event: &str| json!({"event": event, "session": "s-1", "cwd": w.cwd.display().to_string()});
    let mut expected = base("session_start");
    expected["resumed"] = json!(true);
    assert_eq!(json_file(&start), expected);
    let mut expected = base("pre_tool");
    expected["tool"] = json!(TOOL);
    expected["arguments"] = args;
    assert_eq!(json_file(&pre), expected);
    let mut expected = base("post_tool");
    expected["tool"] = json!(TOOL);
    expected["is_error"] = json!(false);
    expected["result"] = json!("the answer");
    assert_eq!(json_file(&post), expected);
}

#[tokio::test]
async fn an_oversized_argument_and_result_are_cut_in_the_payload_and_flagged() {
    // Arrange
    let w = World::new();
    let (pre, post) = (w.out("pre.json"), w.out("post.json"));
    let runner = World::runner(&[
        capture(HookEvent::PreTool, &pre),
        capture(HookEvent::PostTool, &post),
    ]);
    let args = json!({"blob": "a".repeat(MAX_ARGUMENTS_BYTES * 4)});
    let result = "r".repeat(MAX_RESULT_BYTES * 4);

    // Act
    runner
        .pre_tool(&w.session, &w.cwd, TOOL, &args)
        .await
        .unwrap();
    runner
        .post_tool(&w.session, &w.cwd, TOOL, false, &result)
        .await
        .unwrap();

    // Assert
    let (before, after) = (json_file(&pre), json_file(&post));
    assert_eq!(before["arguments_truncated"], true);
    assert_eq!(
        before["arguments"].as_str().unwrap().len(),
        MAX_ARGUMENTS_BYTES
    );
    assert_eq!(after["result_truncated"], true);
    assert_eq!(after["result"].as_str().unwrap().len(), MAX_RESULT_BYTES);
}

#[tokio::test]
async fn a_multibyte_character_at_the_cut_is_dropped_whole() {
    // Arrange
    let w = World::new();
    let (pre, post) = (w.out("pre.json"), w.out("post.json"));
    let runner = World::runner(&[
        capture(HookEvent::PreTool, &pre),
        capture(HookEvent::PostTool, &post),
    ]);
    // Two and three byte characters, so neither cap falls on a boundary.
    let args = json!({"blob": "\u{e9}".repeat(MAX_ARGUMENTS_BYTES)});
    let result = "\u{20ac}".repeat(MAX_RESULT_BYTES);

    // Act
    runner
        .pre_tool(&w.session, &w.cwd, TOOL, &args)
        .await
        .unwrap();
    runner
        .post_tool(&w.session, &w.cwd, TOOL, false, &result)
        .await
        .unwrap();

    // Assert
    let (before, after) = (json_file(&pre), json_file(&post));
    let cut_args = before["arguments"].as_str().unwrap();
    assert!(cut_args.len() < MAX_ARGUMENTS_BYTES, "{}", cut_args.len());
    let cut_result = after["result"].as_str().unwrap();
    assert!(cut_result.len() < MAX_RESULT_BYTES, "{}", cut_result.len());
    assert!(cut_result.chars().all(|c| c == '\u{20ac}'));
}

#[tokio::test]
async fn a_hook_sees_only_the_environment_it_was_given_and_the_session_directory() {
    // Arrange
    let w = World::new();
    let (env_file, cwd_file) = (w.out("env.txt"), w.out("cwd.txt"));
    let script = format!(
        "env > {}\npwd -P > {}",
        env_file.display(),
        cwd_file.display()
    );
    let hooks = [shell(HookEvent::PreTool, &script, false)];
    let runner = World::runner(&hooks);

    // Act
    runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap();

    // Assert
    let seen = std::fs::read_to_string(&env_file).unwrap();
    assert!(seen.contains("PATH=/usr/bin:/bin"), "{seen}");
    // The test process has a home directory and the hook still does not.
    assert!(!seen.contains("HOME="), "{seen}");
    assert!(!seen.contains("CARGO"), "{seen}");
    let cwd = std::fs::read_to_string(&cwd_file).unwrap();
    assert_eq!(
        Path::new(cwd.trim()).canonicalize().unwrap(),
        w.cwd.canonicalize().unwrap()
    );
}

#[tokio::test]
async fn the_command_is_an_argument_list_and_no_shell_reads_it() {
    // Arrange
    let w = World::new();
    let (arg_file, pwned) = (w.out("arg.txt"), w.out("pwned"));
    let body = format!("printf '%s' \"$1\" > {}", arg_file.display());
    let script = w.scratch.script("arg.sh", &body);
    let tricky = format!("a b; touch {} #", pwned.display());
    let runner = World::runner(&[hook(HookEvent::PreTool, &[&script, &tricky], false)]);

    // Act
    runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap();

    // Assert
    assert_eq!(std::fs::read_to_string(&arg_file).unwrap(), tricky);
    assert!(!pwned.exists());
}

#[tokio::test]
async fn a_hook_that_never_reads_standard_input_does_not_hold_up_the_call() {
    // Arrange
    let w = World::new();
    let ignoring = |event| shell(event, "sleep 0.2", true);
    let runner = World::runner(&[ignoring(HookEvent::PreTool), ignoring(HookEvent::PostTool)]);
    let args = json!({"blob": "a".repeat(MAX_ARGUMENTS_BYTES * 4)});
    let result = "r".repeat(MAX_RESULT_BYTES * 4);

    // Act
    let both = async {
        runner.pre_tool(&w.session, &w.cwd, TOOL, &args).await?;
        runner
            .post_tool(&w.session, &w.cwd, TOOL, false, &result)
            .await
    };
    let outcome = tokio::time::timeout(BOUND, both).await;

    // Assert
    assert!(
        outcome
            .expect("a hook that ignores its input held the call")
            .is_ok()
    );
}

#[tokio::test]
async fn a_hook_that_exits_on_its_own_leaves_what_it_started_alone() {
    // Arrange
    let w = World::new();
    let mark = w.out("mark");
    // The leader is reaped when it exits, so its group id may be reused and is not signalled.
    let script = format!(
        "( sleep 1; echo ok > {} ) >/dev/null 2>&1 &",
        mark.display()
    );
    let runner = World::runner(&[shell(HookEvent::PreTool, &script, true)]);

    // Act
    let outcome = runner.pre_tool(&w.session, &w.cwd, TOOL, &json!({})).await;

    // Assert
    assert!(outcome.is_ok());
    poll_until(|| mark.exists()).await;
}

#[tokio::test]
async fn a_process_left_holding_standard_error_is_cut_off_after_the_grace_period() {
    // Arrange
    let w = World::new();
    let (pid_file, mark) = (w.out("left.pid"), w.out("mark"));
    // The leftover inherits standard error and writes to it long after the hook failed.
    let script = format!(
        "( sleep 6; echo late >&2; echo ok > {} ) &\necho $! > {}\nexit 7",
        mark.display(),
        pid_file.display()
    );
    let runner = World::runner(&[shell(HookEvent::PreTool, &script, true)]);

    // Act
    let started = std::time::Instant::now();
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();
    let waited = started.elapsed();

    // Assert
    assert!(blocked.reason.contains("exited with status 7"), "{blocked}");
    assert!(waited < Duration::from_secs(5), "waited {waited:?}");
    // With the reader dropped, the pipe is closed: the leftover's write ends it.
    let leftover = pid_from(&pid_file).await;
    poll_until(|| !process_alive(&leftover)).await;
    assert!(!mark.exists(), "the pipe was still being read");
}

#[tokio::test]
async fn dropping_the_call_kills_the_hooks_process_group() {
    // Arrange
    let w = World::new();
    let pid_file = w.out("hook.pid");
    let script = format!("sleep 30 &\necho $! > {}\nwait", pid_file.display());
    let runner = World::runner(&[shell(HookEvent::PreTool, &script, true)]);

    let args = json!({});

    // Act
    let sleeper = tokio::select! {
        _ = runner.pre_tool(&w.session, &w.cwd, TOOL, &args) => {
            panic!("the hook should still be running")
        }
        pid = pid_from(&pid_file) => pid,
    };

    // Assert
    poll_until(|| !process_alive(&sleeper)).await;
}
