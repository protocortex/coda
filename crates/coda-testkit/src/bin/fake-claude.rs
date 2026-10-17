// SPDX-License-Identifier: AGPL-3.0-only

//! A scripted stand-in for `claude -p --output-format stream-json`.
//!
//! Each user message on stdin produces one turn: a text delta, a permission
//! request for a `Bash` call, then a reply that depends on the answer. The
//! message shapes are synthetic, modelled on the stream-json protocol.
//!
//! A user message containing `crash` makes the process exit mid-turn, and one
//! containing `noise` is preceded by a malformed line and an unknown message
//! type. `stall` parks the process after the text delta, `grandchild` first
//! starts a `sleep` process, `orphan` also closes stdout so the reader sees
//! end of file while the process lives on, and `open-credentials` opens
//! `$HOME/.claude/.credentials.json` for writing and writes one byte, and
//! `echo-text` sends the control file's `echo_text` back as a text delta.
//!
//! Test controls come from `fake-claude-control.json` in the working
//! directory, because the adapter passes the child only an allowlisted
//! environment. `report_path` makes the binary write its arguments and
//! environment facts there, `pid_path` receives the `sleep` pid, and
//! `flood` makes it fill stdout before it reads any input.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const VERSION: &str = "2.1.292 (Claude Code)";
const SESSION_ID: &str = "fake-session-1";
const REQUEST_ID: &str = "req-1";
const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";
const CLAUDECODE_VAR: &str = "CLAUDECODE";
const CONTROL_FILE: &str = "fake-claude-control.json";
const CREDENTIALS_PATH: &str = ".claude/.credentials.json";
const FLOOD_LINES: usize = 20_000;
const GRANDCHILD_PROGRAM: &str = "sleep";
const GRANDCHILD_SECONDS: &str = "300";
const STALL_TRIGGER: &str = "stall";
const GRANDCHILD_TRIGGER: &str = "grandchild";
const ORPHAN_TRIGGER: &str = "orphan";
const CREDENTIALS_TRIGGER: &str = "open-credentials";
const ECHO_TRIGGER: &str = "echo-text";
const PARK_INTERVAL: Duration = Duration::from_secs(3600);
const CRASH_TRIGGER: &str = "crash";
const NOISE_TRIGGER: &str = "noise";
const CRASH_EXIT_CODE: i32 = 3;

fn emit(out: &mut impl Write, value: &serde_json::Value) -> io::Result<()> {
    writeln!(out, "{value}")?;
    out.flush()
}

fn denied(response_line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(response_line)
        .is_ok_and(|v| v["response"]["response"]["behavior"] == "deny")
}

struct Control(serde_json::Value);

impl Control {
    fn load() -> Self {
        let value = std::fs::read_to_string(CONTROL_FILE)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or(serde_json::Value::Null);
        Self(value)
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        self.0[key].as_str().map(PathBuf::from)
    }

    fn text(&self, key: &str) -> Option<&str> {
        self.0[key].as_str()
    }

    fn flood(&self) -> bool {
        self.0["flood"] == true
    }
}

fn start_grandchild(control: &Control) -> io::Result<()> {
    let child = Command::new(GRANDCHILD_PROGRAM)
        .arg(GRANDCHILD_SECONDS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    match control.path("pid_path") {
        Some(path) => std::fs::write(path, child.id().to_string()),
        None => Ok(()),
    }
}

fn open_credentials() -> io::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(home.join(CREDENTIALS_PATH))?;
    file.write_all(b"x")
}

fn close_stdout() -> io::Result<()> {
    rustix::stdio::dup2_stdout(std::fs::File::open("/dev/null")?)?;
    Ok(())
}

fn park() -> ! {
    loop {
        std::thread::sleep(PARK_INTERVAL);
    }
}

fn run_turn(
    user_line: &str,
    lines: &mut impl Iterator<Item = io::Result<String>>,
    out: &mut impl Write,
    control: &Control,
) -> io::Result<()> {
    if user_line.contains(GRANDCHILD_TRIGGER) || user_line.contains(ORPHAN_TRIGGER) {
        start_grandchild(control)?;
    }
    if user_line.contains(CREDENTIALS_TRIGGER) {
        open_credentials()?;
    }
    if user_line.contains(NOISE_TRIGGER) {
        writeln!(out, "{{not json")?;
        emit(out, &serde_json::json!({"type":"mystery"}))?;
    }
    emit(
        out,
        &serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta",
            "delta":{"type":"text_delta","text":"working"}}}),
    )?;
    if user_line.contains(ECHO_TRIGGER)
        && let Some(text) = control.text("echo_text")
    {
        emit(
            out,
            &serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta",
                "delta":{"type":"text_delta","text":text}}}),
        )?;
    }
    if user_line.contains(CRASH_TRIGGER) {
        std::process::exit(CRASH_EXIT_CODE);
    }
    if user_line.contains(ORPHAN_TRIGGER) {
        close_stdout()?;
    }
    if user_line.contains(STALL_TRIGGER)
        || user_line.contains(GRANDCHILD_TRIGGER)
        || user_line.contains(ORPHAN_TRIGGER)
    {
        park();
    }
    emit(
        out,
        &serde_json::json!({"type":"control_request","request_id":REQUEST_ID,
            "request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"}}}),
    )?;
    let Some(response) = lines.next().transpose()? else {
        return Ok(());
    };
    let text = if denied(&response) { "denied" } else { "ran" };
    emit(
        out,
        &serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}}),
    )?;
    emit(out, &serde_json::json!({"type":"result","is_error":false}))
}

fn write_report(path: PathBuf) -> io::Result<()> {
    let report = serde_json::json!({
        "args": std::env::args().skip(1).collect::<Vec<_>>(),
        "anthropic_api_key_present": std::env::var_os(API_KEY_VAR).is_some(),
        "claudecode_present": std::env::var_os(CLAUDECODE_VAR).is_some(),
        "home": std::env::var("HOME").ok(),
    });
    std::fs::write(path, report.to_string())
}

fn main() -> io::Result<()> {
    if std::env::args().any(|arg| arg == "--version") {
        println!("{VERSION}");
        return Ok(());
    }
    let control = Control::load();
    if let Some(path) = control.path("report_path") {
        write_report(path)?;
    }
    let mut out = io::stdout().lock();
    if control.flood() {
        for _ in 0..FLOOD_LINES {
            emit(&mut out, &serde_json::json!({"type":"mystery"}))?;
        }
    }
    emit(
        &mut out,
        &serde_json::json!({"type":"system","subtype":"env_report",
            "anthropic_api_key_present":std::env::var_os(API_KEY_VAR).is_some(),
            "claudecode_present":std::env::var_os(CLAUDECODE_VAR).is_some()}),
    )?;
    emit(
        &mut out,
        &serde_json::json!({"type":"system","subtype":"init","session_id":SESSION_ID}),
    )?;
    let mut lines = io::stdin().lock().lines();
    while let Some(line) = lines.next().transpose()? {
        if !line.trim().is_empty() {
            run_turn(&line, &mut lines, &mut out, &control)?;
        }
    }
    Ok(())
}
