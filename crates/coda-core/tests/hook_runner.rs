// SPDX-License-Identifier: AGPL-3.0-only

//! The hook runner on its own: what a hook is given, what stops it, what it
//! cannot see and what it leaves behind. Every hook is a small script in a
//! temporary directory or a `/bin/sh -c` line.

mod common;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use coda_config::hooks::{HookConfig, HookEvent};
use coda_core::hooks::{HookRunner, MAX_ARGUMENTS_BYTES, MAX_REASON_BYTES, MAX_RESULT_BYTES};
use coda_protocol::types::SessionId;
use common::{
    BOUND, PATIENT_MS, STARTED_MS, Scratch, capture_logs, hook, json_file, pid_from, poll_until,
    process_alive, shell, timed,
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
    assert_eq!(blocked.hook, format!("hooks[0] ({script})"));
    // The cause comes first and the lines are folded onto one.
    assert_eq!(
        blocked.reason,
        "exited with status 3: first line no deploys on friday"
    );
    assert!(blocked.to_string().contains("pre_tool"), "{blocked}");
}

#[tokio::test]
async fn a_blocking_reason_keeps_how_the_hook_ended_in_front_of_what_it_said() {
    // Arrange
    let w = World::new();
    let slow = timed(
        shell(HookEvent::PreTool, "echo checking... >&2\nsleep 30", true),
        STARTED_MS,
    );
    let runner = World::runner(&[slow]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    let expected = format!("timed out after {STARTED_MS} ms: checking...");
    assert_eq!(blocked.reason, expected);
}

#[tokio::test]
async fn a_hook_cannot_forge_a_log_line_or_move_the_cursor_through_its_reason() {
    // Arrange
    let (logs, _guard) = capture_logs();
    let w = World::new();
    let forged = r"printf 'x\n2026-10-23 INFO forged line\033[31m\r\n' >&2; exit 1";
    let runner = World::runner(&[
        shell(HookEvent::PostTool, forged, false),
        shell(HookEvent::PreTool, forged, true),
    ]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();
    runner
        .post_tool(&w.session, &w.cwd, TOOL, false, "ran")
        .await
        .unwrap();

    // Assert
    let expected = "exited with status 1: x 2026-10-23 INFO forged line[31m";
    assert_eq!(blocked.reason, expected);
    assert!(
        !blocked.to_string().contains(char::is_control),
        "{blocked:?}"
    );
    let logged = logs.text();
    assert!(
        logged.contains(&format!("reason={expected}\n")),
        "{logged:?}"
    );
    assert!(!logged.contains('\u{1b}'), "{logged:?}");
}

#[tokio::test]
async fn format_characters_that_hide_or_reorder_text_are_dropped_from_a_reason() {
    // Arrange
    let w = World::new();
    // A right-to-left override (U+202E), a zero width space (U+200B), a byte
    // order mark (U+FEFF) and an isolate (U+2066), as UTF-8 octal escapes.
    let hidden = r"printf 'a\342\200\256b\342\200\213c\357\273\277d\342\201\246e' >&2; exit 1";
    let runner = World::runner(&[shell(HookEvent::PreTool, hidden, true)]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    assert_eq!(blocked.reason, "exited with status 1: abcde");
}

#[tokio::test]
async fn a_block_and_a_log_line_name_the_hook_by_its_place_in_the_config() {
    // Arrange
    let (logs, _guard) = capture_logs();
    let w = World::new();
    let runner = World::runner(&[
        shell(HookEvent::PostTool, "exit 1", true),
        shell(HookEvent::PreTool, "exit 2", false),
        shell(HookEvent::PreTool, "exit 3", true),
    ]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    assert_eq!(blocked.hook, "hooks[2] (/bin/sh)");
    assert!(
        blocked.to_string().contains("`hooks[2] (/bin/sh)`"),
        "{blocked}"
    );
    let logged = logs.text();
    assert!(
        logged.contains("hook=hooks[1] (/bin/sh) reason=exited with status 2"),
        "{logged}"
    );
}

#[tokio::test]
async fn a_hook_that_cannot_start_blocks_when_blocking() {
    // Arrange
    let w = World::new();
    let runner = World::runner(&[hook(HookEvent::PreTool, &["/nonexistent/hook"], true)]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    assert_eq!(blocked.hook, "hooks[0] (/nonexistent/hook)");
    assert!(blocked.reason.starts_with("could not start: "), "{blocked}");
}

#[tokio::test]
async fn a_hook_that_cannot_start_is_logged_when_not_blocking() {
    // Arrange
    let (logs, _guard) = capture_logs();
    let w = World::new();
    let runner = World::runner(&[hook(HookEvent::PreTool, &["/nonexistent/hook"], false)]);

    // Act
    let outcome = runner.pre_tool(&w.session, &w.cwd, TOOL, &json!({})).await;

    // Assert
    assert!(outcome.is_ok());
    let logged = logs.text();
    assert!(
        logged.contains("hook=hooks[0] (/nonexistent/hook) reason=could not start: "),
        "{logged}"
    );
}

#[tokio::test]
async fn hooks_run_in_order_and_the_first_failing_blocking_one_ends_the_run() {
    // Arrange
    let w = World::new();
    let (order, mark) = (w.out("order.txt"), w.out("mark"));
    let step = |name: &str, status: i32, blocking: bool| {
        let script = format!("echo {name} >> {}; exit {status}", order.display());
        shell(HookEvent::PreTool, &script, blocking)
    };
    let last = format!(
        "echo last >> {0}; touch {1}",
        order.display(),
        mark.display()
    );
    let runner = World::runner(&[
        step("first", 1, false),
        step("second", 2, true),
        shell(HookEvent::PreTool, &last, true),
    ]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    assert_eq!(blocked.hook, "hooks[1] (/bin/sh)");
    assert_eq!(std::fs::read_to_string(&order).unwrap(), "first\nsecond\n");
    assert!(!mark.exists(), "a hook after the blocking failure ran");
}

#[tokio::test]
async fn a_bare_program_name_is_found_on_the_path_the_hook_was_given() {
    // Arrange
    let w = World::new();
    let mark = w.out("found");
    w.scratch
        .script("guard-by-name", &format!("touch {}", mark.display()));
    let path = format!("{}:/usr/bin:/bin", w.scratch.dir.path().display());
    let env = vec![(OsString::from("PATH"), OsString::from(path))];
    let runner = HookRunner::new(&[hook(HookEvent::PreTool, &["guard-by-name"], true)], env);

    // Act
    let outcome = runner.pre_tool(&w.session, &w.cwd, TOOL, &json!({})).await;

    // Assert
    assert!(outcome.is_ok(), "{outcome:?}");
    assert!(mark.exists());
}

/// An executable `name` in `dir` that touches `mark`.
fn marker_program(dir: &Path, name: &str, mark: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let program = dir.join(name);
    std::fs::write(&program, format!("#!/bin/sh\ntouch {}\n", mark.display())).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn runner_on_path(hooks: &[HookConfig], path: &str) -> HookRunner {
    HookRunner::new(hooks, vec![(OsString::from("PATH"), OsString::from(path))])
}

#[tokio::test]
async fn an_empty_or_relative_path_entry_never_finds_a_program_in_the_session_directory() {
    // Arrange
    let w = World::new();
    let tools = w.out("tools");
    let (from_cwd, from_bin, from_tools) =
        (w.out("cwd.mark"), w.out("bin.mark"), w.out("tools.mark"));
    // The session directory is the one place the model can write.
    marker_program(&w.cwd, "guard", &from_cwd);
    marker_program(&w.cwd.join("bin"), "guard", &from_bin);
    marker_program(&tools, "guard", &from_tools);
    let hooks = [hook(HookEvent::PreTool, &["guard"], true)];
    // The rest of the path is for `touch`, which the programs call.
    let dir = format!("{}", tools.display());
    let rest = "/usr/bin:/bin";

    // Act
    for path in [
        format!(":{dir}:{rest}"),
        format!(".:{dir}:{rest}"),
        format!("bin:{dir}:{rest}"),
        format!("./bin:{dir}:{rest}"),
    ] {
        let outcome = runner_on_path(&hooks, &path)
            .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
            .await;
        assert!(outcome.is_ok(), "{path}: {outcome:?}");
    }

    // Assert
    assert!(from_tools.exists(), "the absolute entry was not used");
    assert!(!from_cwd.exists(), "a guard of the session directory ran");
    assert!(!from_bin.exists(), "a guard of a relative entry ran");
}

#[tokio::test]
async fn a_bare_name_found_only_through_an_empty_or_relative_path_entry_fails_to_start() {
    // Arrange
    let w = World::new();
    let (from_cwd, from_bin) = (w.out("cwd.mark"), w.out("bin.mark"));
    marker_program(&w.cwd, "coda-test-guard", &from_cwd);
    marker_program(&w.cwd.join("bin"), "coda-test-guard", &from_bin);
    let runner = runner_on_path(
        &[hook(HookEvent::PreTool, &["coda-test-guard"], true)],
        ":.:bin:/usr/bin:/bin",
    );

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    assert!(blocked.reason.starts_with("could not start: "), "{blocked}");
    assert!(!from_cwd.exists(), "a guard of the session directory ran");
    assert!(!from_bin.exists(), "a guard of a relative entry ran");
}

#[tokio::test]
async fn a_relative_program_path_is_not_run_even_when_the_config_check_was_skipped() {
    // Arrange
    let w = World::new();
    let from_cwd = w.out("cwd.mark");
    marker_program(&w.cwd, "guard", &from_cwd);
    let runner = World::runner(&[hook(HookEvent::PreTool, &["./guard"], true)]);

    // Act
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();

    // Assert
    assert!(blocked.reason.starts_with("could not start: "), "{blocked}");
    assert!(!from_cwd.exists(), "a guard of the session directory ran");
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
    let said = blocked
        .reason
        .strip_prefix("exited with status 1: ")
        .expect("the cause comes first");
    // Whole characters only, so at most two bytes of the window are dropped.
    assert!(said.len() <= MAX_REASON_BYTES, "{}", said.len());
    assert!(said.len() >= MAX_REASON_BYTES - 2, "{}", said.len());
    assert!(!said.contains("HEAD-MARKER"), "the start was kept");
    assert!(!said.contains('\u{fffd}'), "the cut split a character");
    let body = said.strip_suffix("END").expect("the end was kept");
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
        "hook failed, continuing: event=post_tool hook=hooks[0] ({script}) reason=exited with status 7: disk is full"
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
async fn large_arguments_and_a_large_result_reach_the_hook_whole() {
    // Arrange
    let w = World::new();
    let (pre, post) = (w.out("pre.json"), w.out("post.json"));
    let runner = World::runner(&[
        capture(HookEvent::PreTool, &pre),
        capture(HookEvent::PostTool, &post),
    ]);
    // The key a guard looks for comes last, behind more text than the old cut kept.
    let args = json!({
        "old_string": "a".repeat(9 * 1024),
        "new_string": "b".repeat(9 * 1024),
        "path": "protected.txt",
    });
    // More than the daemon compacts, which the model still sees whole.
    let result = format!("{}SECRET", "r".repeat(20 * 1024));

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
    assert_eq!(before["arguments"], args);
    assert!(before.get("arguments_truncated").is_none(), "{before}");
    assert_eq!(after["result"], result.as_str());
    assert!(after.get("result_truncated").is_none(), "{after}");
}

#[tokio::test]
async fn an_oversized_call_is_cut_and_flagged_for_a_hook_that_is_not_blocking() {
    // Arrange
    let w = World::new();
    let (pre, post) = (w.out("pre.json"), w.out("post.json"));
    let runner = World::runner(&[
        capture(HookEvent::PreTool, &pre),
        capture(HookEvent::PostTool, &post),
    ]);
    let args = json!({"blob": "a".repeat(MAX_ARGUMENTS_BYTES * 2)});
    let result = "r".repeat(MAX_RESULT_BYTES * 2);

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
async fn an_oversized_call_blocks_a_blocking_hook_without_running_it() {
    // Arrange
    let w = World::new();
    let ran = w.out("ran");
    let guard = |event| shell(event, &format!("touch {}", ran.display()), true);
    let runner = World::runner(&[guard(HookEvent::PreTool), guard(HookEvent::PostTool)]);
    let args = json!({"blob": "a".repeat(MAX_ARGUMENTS_BYTES * 2)});
    let result = "r".repeat(MAX_RESULT_BYTES * 2);

    // Act
    let before = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &args)
        .await
        .unwrap_err();
    let after = runner
        .post_tool(&w.session, &w.cwd, TOOL, false, &result)
        .await
        .unwrap_err();

    // Assert
    let arguments = format!(
        "the arguments are too large for the hook to check (over {MAX_ARGUMENTS_BYTES} bytes)"
    );
    let result =
        format!("the result is too large for the hook to check (over {MAX_RESULT_BYTES} bytes)");
    for (blocked, hook, reason) in [
        (&before, "hooks[0] (/bin/sh)", &arguments),
        (&after, "hooks[1] (/bin/sh)", &result),
    ] {
        assert_eq!(&blocked.reason, reason);
        assert_eq!(blocked.hook, hook);
    }
    assert!(
        !ran.exists(),
        "a guard ran on a payload it could not see whole"
    );
}

#[tokio::test]
async fn a_call_just_under_the_cap_is_checked_whole() {
    // Arrange
    let w = World::new();
    let seen = w.out("seen.json");
    let runner = World::runner(&[shell(
        HookEvent::PostTool,
        &format!("cat > {}", seen.display()),
        true,
    )]);
    let result = "r".repeat(MAX_RESULT_BYTES);

    // Act
    let outcome = runner
        .post_tool(&w.session, &w.cwd, TOOL, false, &result)
        .await;

    // Assert
    assert!(outcome.is_ok(), "{outcome:?}");
    assert_eq!(json_file(&seen)["result"], result.as_str());
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

/// Past what a pipe holds, so a write to a hook that does not read must wait.
const PAST_THE_PIPE_BYTES: usize = 128 * 1024;

/// Runs `script` as a blocking hook of each tool event, with a payload past the
/// pipe's buffer, and says whether both calls passed.
async fn passes_a_big_payload(script: &str, timeout_ms: u64) -> bool {
    let w = World::new();
    let hooked = |event| timed(shell(event, script, true), timeout_ms);
    let runner = World::runner(&[hooked(HookEvent::PreTool), hooked(HookEvent::PostTool)]);
    let args = json!({"blob": "a".repeat(PAST_THE_PIPE_BYTES)});
    // A control character takes six bytes once it is escaped in the payload.
    let result = "\u{1}".repeat(MAX_RESULT_BYTES);
    let both = async {
        runner.pre_tool(&w.session, &w.cwd, TOOL, &args).await?;
        runner
            .post_tool(&w.session, &w.cwd, TOOL, false, &result)
            .await
    };
    let outcome = tokio::time::timeout(BOUND, both)
        .await
        .expect("a hook that ignores its input held the call");
    outcome.is_ok()
}

#[tokio::test]
async fn a_hook_that_never_reads_standard_input_does_not_hold_up_the_call() {
    assert!(passes_a_big_payload("sleep 0.2", PATIENT_MS).await);
}

#[tokio::test]
async fn a_hook_that_closes_standard_input_first_is_not_a_failure() {
    assert!(passes_a_big_payload("exec 0<&-\nsleep 0.5", PATIENT_MS).await);
}

#[tokio::test]
async fn a_hook_that_exits_while_a_leftover_holds_its_input_open_is_not_waited_out() {
    // The leftover keeps the read end of the pipe open for longer than the
    // hook's limit, so a writer that waits for the pipe to drain times out.
    let script = "sleep 4 <&0 >/dev/null 2>&1 &\nexit 0";
    assert!(passes_a_big_payload(script, STARTED_MS).await);
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
async fn what_a_failed_hook_said_is_kept_when_a_leftover_holds_standard_error_open() {
    // Arrange
    let w = World::new();
    // The message is written in full, then a process that outlives the grace
    // period is left holding standard error.
    let script = "echo 'no deploys on friday' >&2\nsleep 6 &\nexit 1";
    let runner = World::runner(&[shell(HookEvent::PreTool, script, true)]);

    // Act
    let started = std::time::Instant::now();
    let blocked = runner
        .pre_tool(&w.session, &w.cwd, TOOL, &json!({}))
        .await
        .unwrap_err();
    let waited = started.elapsed();

    // Assert
    assert_eq!(blocked.reason, "exited with status 1: no deploys on friday");
    assert!(waited < Duration::from_secs(5), "waited {waited:?}");
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
