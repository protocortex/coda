// SPDX-License-Identifier: AGPL-3.0-only

//! A scripted stand-in for `codex exec --json`.
//!
//! One run is one turn: the prompt arrives on stdin, then the binary prints
//! `thread.started`, `turn.started`, an agent message, a command item and
//! `turn.completed`. The event shapes are SYNTHETIC, written from the
//! documented JSONL event names, not recorded from a real `codex` run.
//!
//! A prompt containing `crash` exits mid-turn and `fail` ends the turn with
//! `turn.failed`. `noise` is preceded by a malformed line and an unknown
//! event type, `bigline` by one line over the parser limit. `hostile` reports a thread id that looks like a flag. `stall` parks
//! the process after `turn.started` and `grandchild` first starts a `sleep`
//! process.
//!
//! Test controls come from `fake-codex-control.json` in the working
//! directory, because the adapter passes the child only an allowlisted
//! environment. `report_path` receives the arguments, the prompt and
//! environment facts, and `pid_path` receives the `sleep` pid.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const VERSION: &str = "codex-cli 0.0.0-fake";
const NEW_THREAD_ID: &str = "fake-thread-1";
const RESUME_ARG: &str = "resume";
const SANDBOX_FLAG: &str = "--sandbox";
const HOSTILE_THREAD_ID: &str = "--dangerously-bypass-approvals-and-sandbox";
const CONTROL_FILE: &str = "fake-codex-control.json";
const GRANDCHILD_PROGRAM: &str = "sleep";
const GRANDCHILD_SECONDS: &str = "300";
const BIG_LINE_BYTES: usize = 9 * 1024 * 1024;
const PARK_INTERVAL: Duration = Duration::from_secs(3600);
const CRASH_EXIT_CODE: i32 = 3;

fn emit(out: &mut impl Write, value: &Value) -> io::Result<()> {
    writeln!(out, "{value}")?;
    out.flush()
}

fn control_path(key: &str) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(CONTROL_FILE).ok()?;
    let control: Value = serde_json::from_str(&raw).ok()?;
    control[key].as_str().map(PathBuf::from)
}

fn start_grandchild() -> io::Result<()> {
    let child = Command::new(GRANDCHILD_PROGRAM)
        .arg(GRANDCHILD_SECONDS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    match control_path("pid_path") {
        Some(path) => std::fs::write(path, child.id().to_string()),
        None => Ok(()),
    }
}

fn park() -> ! {
    loop {
        std::thread::sleep(PARK_INTERVAL);
    }
}

fn arg_after(args: &[String], name: &str) -> Option<String> {
    let at = args.iter().position(|arg| arg == name)?;
    args.get(at + 1).cloned()
}

fn write_report(args: &[String], prompt: &str) -> io::Result<()> {
    let Some(path) = control_path("report_path") else {
        return Ok(());
    };
    let report = json!({
        "pid": std::process::id(),
        "args": args,
        "prompt": prompt,
        "sandbox": arg_after(args, SANDBOX_FLAG),
        "env_names": std::env::vars_os()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        "home": std::env::var("HOME").ok(),
    });
    std::fs::write(path, report.to_string())
}

fn run_turn(args: &[String], prompt: &str, out: &mut impl Write) -> io::Result<()> {
    let thread_id = if prompt.contains("hostile") {
        HOSTILE_THREAD_ID.to_owned()
    } else {
        arg_after(args, RESUME_ARG).unwrap_or_else(|| NEW_THREAD_ID.to_owned())
    };
    if prompt.contains("grandchild") {
        start_grandchild()?;
    }
    if prompt.contains("noise") {
        writeln!(out, "{{not json")?;
        emit(out, &json!({"type": "mystery"}))?;
    }
    if prompt.contains("bigline") {
        writeln!(out, "{}", "x".repeat(BIG_LINE_BYTES))?;
    }
    emit(
        out,
        &json!({"type": "thread.started", "thread_id": thread_id}),
    )?;
    emit(out, &json!({"type": "turn.started"}))?;
    if prompt.contains("crash") {
        std::process::exit(CRASH_EXIT_CODE);
    }
    if prompt.contains("stall") || prompt.contains("grandchild") {
        park();
    }
    if prompt.contains("fail") {
        return emit(
            out,
            &json!({"type": "turn.failed", "error": {"message": "synthetic failure"}}),
        );
    }
    emit(
        out,
        &json!({"type": "item.completed", "item": {"id": "item_0", "type": "agent_message",
            "text": "working on it"}}),
    )?;
    emit(
        out,
        &json!({"type": "item.completed", "item": {"id": "item_1", "type": "command_execution",
            "command": "ls", "aggregated_output": "", "exit_code": 0, "status": "completed"}}),
    )?;
    emit(
        out,
        &json!({"type": "turn.completed", "usage": {"input_tokens": 120,
            "cached_input_tokens": 100, "output_tokens": 30}}),
    )
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--version") {
        println!("{VERSION}");
        return Ok(());
    }
    let mut prompt = String::new();
    io::stdin().lock().read_to_string(&mut prompt)?;
    write_report(&args, &prompt)?;
    run_turn(&args, &prompt, &mut io::stdout().lock())
}
