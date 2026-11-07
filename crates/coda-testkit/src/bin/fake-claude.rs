// SPDX-License-Identifier: Apache-2.0

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
//! `echo-text` sends the control file's `echo_text` back as a text delta, and `delta_burst`
//! makes every turn send that many one-character text deltas before it goes on.
//! `askdie` exits right after it asks for permission, before any answer, and
//! `tooldie` exits right after the tool call is answered, and `vanish` deletes
//! its own binary before crashing like `crash`, so it cannot be started again.
//! `tool-slow` runs a tool that needs no permission: it announces a `tool_use`,
//! stays silent for `TOOL_SLOW`, then reports the `tool_result` and finishes,
//! and `tool-hang` announces one and never reports it. `tool-flaky` announces a tool and
//! crashes the first time (the control file's `crash_marker` remembers it), then stalls.
//! `tool-quick` runs one that succeeds at once and `tool-error` one that fails at once.
//! `report-usage` reports a rate limit, then finishes with token usage and a context window on the result.
//! `interruptible` announces a tool and waits for an `interrupt` control request, answers it
//! and ends the turn with an error result, as claude does, then takes the next message.
//! `hit-limit` reports a rejected seven-day limit, then ends the turn with an error.
//! `stream-long` streams `input_json_delta` and `thinking_delta` lines for longer than
//! the idle limits the tests use, then finishes.
//! `flaky` crashes like `crash` the first time and works afterwards, using
//! the control file's `crash_marker` path to remember it already crashed.
//!
//! Test controls come from `fake-claude-control.json` in the working
//! directory, because the adapter passes the child only an allowlisted
//! environment. `report_path` makes the binary write its arguments and
//! environment facts there, `pid_path` receives the `sleep` pid, `pid_log` collects the pid of every
//! `sleep` it started, one per line, and
//! `start_log` collects the pid of every process start, one per line, and
//! `flood` makes it fill stdout before it reads any input, and `start_gate`
//! holds the start-up, before any output, until a file exists at that path, and
//! exits non-zero if none appears within two minutes. `reject_resume` makes it exit
//! at once, before any output, when it is started with `--resume`, and
//! `reject_resume_of` does it only for a start that resumes that id, and `resume_crash_marker` does the same only the first time (the file remembers it). `prompt_path`
//! appends every user line it reads to that file, and `replies` is a list of
//! `{"when": <text>, "text": <reply>}` entries: a user line that contains `when` is answered
//! with the `text` of the first such entry as a text delta, so two agents in one run can give
//! different answers. `init` is an object whose fields are added to the `init` message,
//! such as `mcp_servers` or `skills`.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const VERSION: &str = "2.1.292 (Claude Code)";
const SESSION_ID: &str = "fake-session-1";
const REQUEST_ID: &str = "req-1";
const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";
const CLAUDECODE_VAR: &str = "CLAUDECODE";
const CONTROL_FILE: &str = "fake-claude-control.json";
const CREDENTIALS_PATH: &str = ".claude/.credentials.json";
const GATE_POLL: Duration = Duration::from_millis(10);
/// Longest a `start_gate` holds the start-up, so an orphaned fake cannot poll forever.
const GATE_LIMIT: Duration = Duration::from_secs(120);
const FLOOD_LINES: usize = 20_000;
const GRANDCHILD_PROGRAM: &str = "sleep";
const GRANDCHILD_SECONDS: &str = "300";
const STALL_TRIGGER: &str = "stall";
const GRANDCHILD_TRIGGER: &str = "grandchild";
const ORPHAN_TRIGGER: &str = "orphan";
const CREDENTIALS_TRIGGER: &str = "open-credentials";
const ECHO_TRIGGER: &str = "echo-text";
const PARK_INTERVAL: Duration = Duration::from_secs(3600);
const BURST_CHAR: &str = "x";
const RESUME_FLAG: &str = "--resume";
const CRASH_TRIGGER: &str = "crash";
const FLAKY_TRIGGER: &str = "flaky";
const TOOL_DIE_TRIGGER: &str = "tooldie";
const ASK_DIE_TRIGGER: &str = "askdie";
const VANISH_TRIGGER: &str = "vanish";
const NOISE_TRIGGER: &str = "noise";
const CRASH_EXIT_CODE: i32 = 3;
const TOOL_SLOW_TRIGGER: &str = "tool-slow";
const TOOL_HANG_TRIGGER: &str = "tool-hang";
const TOOL_FLAKY_TRIGGER: &str = "tool-flaky";
const STREAM_LONG_TRIGGER: &str = "stream-long";
const TOOL_QUICK_TRIGGER: &str = "tool-quick";
const TOOL_ERROR_TRIGGER: &str = "tool-error";
const REPORT_USAGE_TRIGGER: &str = "report-usage";
const HIT_LIMIT_TRIGGER: &str = "hit-limit";
const INTERRUPTIBLE_TRIGGER: &str = "interruptible";
const INTERRUPT_SUBTYPE: &str = "interrupt";
const STREAM_STEP: Duration = Duration::from_millis(100);
const STREAM_STEPS: u32 = 15;
const TOOL_USE_ID: &str = "toolu-1";
/// Silent time of a slow tool: past the idle limits the tests use.
const TOOL_SLOW: Duration = Duration::from_millis(1500);

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

    /// The reply of the first `replies` entry whose `when` the user line contains.
    fn reply_for(&self, user_line: &str) -> Option<&str> {
        self.0["replies"].as_array()?.iter().find_map(|entry| {
            let when = entry["when"].as_str()?;
            user_line
                .contains(when)
                .then(|| entry["text"].as_str())
                .flatten()
        })
    }

    fn flood(&self) -> bool {
        self.0["flood"] == true
    }

    fn delta_burst(&self) -> u64 {
        self.0["delta_burst"].as_u64().unwrap_or(0)
    }

    fn rejects_resume(&self) -> bool {
        self.0["reject_resume"] == true
    }

    /// Whether this start resumes the one id `reject_resume_of` names.
    fn rejects_this_resume(&self) -> bool {
        let Some(refused) = self.text("reject_resume_of") else {
            return false;
        };
        let args: Vec<String> = std::env::args().collect();
        args.iter()
            .position(|arg| arg == RESUME_FLAG)
            .and_then(|at| args.get(at + 1))
            .is_some_and(|id| id == refused)
    }
}

fn start_grandchild(control: &Control) -> io::Result<()> {
    let child = Command::new(GRANDCHILD_PROGRAM)
        .arg(GRANDCHILD_SECONDS)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(log) = control.path("pid_log") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        writeln!(file, "{}", child.id())?;
    }
    match control.path("pid_path") {
        Some(path) => std::fs::write(path, child.id().to_string()),
        None => Ok(()),
    }
}

/// Whether this is the first `flaky` run, which is recorded by creating the marker.
fn crashes_once(user_line: &str, control: &Control) -> io::Result<bool> {
    let Some(marker) = control.path("crash_marker") else {
        return Ok(false);
    };
    if !user_line.contains(FLAKY_TRIGGER) || marker.exists() {
        return Ok(false);
    }
    std::fs::write(marker, b"crashed")?;
    Ok(true)
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

#[expect(
    clippy::disallowed_methods,
    reason = "the fake provider simulates provider latency in real time"
)]
fn park() -> ! {
    loop {
        std::thread::sleep(PARK_INTERVAL);
    }
}

/// The turns that end in a fixed script, chosen by a word in the user line.
fn scripted_ending(
    user_line: &str,
    lines: &mut impl Iterator<Item = io::Result<String>>,
    out: &mut impl Write,
    control: &Control,
) -> Option<io::Result<()>> {
    if user_line.contains(TOOL_FLAKY_TRIGGER) {
        return Some(run_tool_flaky(control, out));
    }
    if user_line.contains(STREAM_LONG_TRIGGER) {
        return Some(stream_long(out));
    }
    if user_line.contains(TOOL_QUICK_TRIGGER) || user_line.contains(TOOL_ERROR_TRIGGER) {
        return Some(run_quick_tool(user_line.contains(TOOL_ERROR_TRIGGER), out));
    }
    if user_line.contains(REPORT_USAGE_TRIGGER) {
        return Some(report_usage(out));
    }
    if user_line.contains(HIT_LIMIT_TRIGGER) {
        return Some(hit_limit(out));
    }
    if user_line.contains(INTERRUPTIBLE_TRIGGER) {
        return Some(run_interruptible(lines, out));
    }
    None
}

fn run_turn(
    user_line: &str,
    lines: &mut impl Iterator<Item = io::Result<String>>,
    out: &mut impl Write,
    control: &Control,
) -> io::Result<()> {
    if let Some(path) = control.path("prompt_path") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{user_line}")?;
    }
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
    for _ in 0..control.delta_burst() {
        emit(
            out,
            &serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta",
                "delta":{"type":"text_delta","text":BURST_CHAR}}}),
        )?;
    }
    if user_line.contains(ECHO_TRIGGER)
        && let Some(text) = control.text("echo_text")
    {
        emit(
            out,
            &serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta",
                "delta":{"type":"text_delta","text":text}}}),
        )?;
    }
    if let Some(text) = control.reply_for(user_line) {
        emit(
            out,
            &serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta",
                "delta":{"type":"text_delta","text":text}}}),
        )?;
    }
    if let Some(done) = scripted_ending(user_line, lines, out, control) {
        return done;
    }
    if user_line.contains(VANISH_TRIGGER) {
        std::fs::remove_file(std::env::current_exe()?)?;
        std::process::exit(CRASH_EXIT_CODE);
    }
    if user_line.contains(CRASH_TRIGGER) || crashes_once(user_line, control)? {
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
    if user_line.contains(TOOL_SLOW_TRIGGER) || user_line.contains(TOOL_HANG_TRIGGER) {
        return run_unasked_tool(user_line, out);
    }
    emit(
        out,
        &serde_json::json!({"type":"control_request","request_id":REQUEST_ID,
            "request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},
                "description":"List the files"}}),
    )?;
    if user_line.contains(ASK_DIE_TRIGGER) {
        std::process::exit(CRASH_EXIT_CODE);
    }
    let Some(response) = lines.next().transpose()? else {
        return Ok(());
    };
    if user_line.contains(TOOL_DIE_TRIGGER) {
        std::process::exit(CRASH_EXIT_CODE);
    }
    let text = if denied(&response) { "denied" } else { "ran" };
    emit(
        out,
        &serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}}),
    )?;
    emit(out, &serde_json::json!({"type":"result","is_error":false}))
}

/// Announces a tool, then waits for an `interrupt` request, answers it and ends the turn in error.
fn run_interruptible(
    lines: &mut impl Iterator<Item = io::Result<String>>,
    out: &mut impl Write,
) -> io::Result<()> {
    emit(
        out,
        &serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":TOOL_USE_ID,"name":"Bash","input":{"command":"ls"}}]}}),
    )?;
    while let Some(line) = lines.next().transpose()? {
        let request: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        if request["request"]["subtype"] == INTERRUPT_SUBTYPE {
            emit(
                out,
                &serde_json::json!({"type":"control_response","response":{
                    "subtype":"success","request_id":request["request_id"]}}),
            )?;
            return emit(
                out,
                &serde_json::json!({"type":"result","subtype":"error_during_execution","is_error":true}),
            );
        }
    }
    Ok(())
}

/// Sends a rate-limit report and a result with usage, in the shapes claude 2.1.296 wrote.
fn report_usage(out: &mut impl Write) -> io::Result<()> {
    emit(
        out,
        &serde_json::json!({"type":"rate_limit_event","rate_limit_info":{
            "status":"allowed","rateLimitType":"seven_day","resetsAt":1_794_193_200_u64,
            "unifiedWindows":{"seven_day":{"utilization":0.09,"resetsAt":1_794_193_200_u64}}}}),
    )?;
    emit(
        out,
        &serde_json::json!({"type":"result","is_error":false,"usage":{
            "input_tokens":3,"output_tokens":11,
            "cache_read_input_tokens":7,"cache_creation_input_tokens":5,
            "iterations":[
                {"input_tokens":2,"cache_read_input_tokens":1,"cache_creation_input_tokens":4},
                {"input_tokens":1,"cache_read_input_tokens":6,"cache_creation_input_tokens":1}]},
            "modelUsage":{"fake-model":{"contextWindow":200_000}}}),
    )
}

/// Reports a rejected rate limit and ends the turn in error, as claude does at the plan limit.
fn hit_limit(out: &mut impl Write) -> io::Result<()> {
    emit(
        out,
        &serde_json::json!({"type":"rate_limit_event","rate_limit_info":{
            "status":"rejected","rateLimitType":"seven_day_opus","resetsAt":1_794_193_200_u64,
            "unifiedWindows":{"seven_day_opus":{"utilization":1.0,"resetsAt":1_794_193_200_u64}}}}),
    )?;
    emit(out, &serde_json::json!({"type":"result","is_error":true}))
}

/// Announces a tool and crashes once; after the restart nothing is in flight and it stalls.
fn run_tool_flaky(control: &Control, out: &mut impl Write) -> io::Result<()> {
    match control.path("crash_marker") {
        Some(marker) if !marker.exists() => {
            std::fs::write(marker, b"crashed")?;
            emit(
                out,
                &serde_json::json!({"type":"assistant","message":{"content":[
                    {"type":"tool_use","id":TOOL_USE_ID,"name":"Bash","input":{"command":"ls"}}]}}),
            )?;
            std::process::exit(CRASH_EXIT_CODE)
        }
        _ => park(),
    }
}

/// Streams tool input and thinking, which carry no text, for longer than the idle limits.
#[expect(
    clippy::disallowed_methods,
    reason = "the fake provider simulates provider latency in real time"
)]
fn stream_long(out: &mut impl Write) -> io::Result<()> {
    for step in 0..STREAM_STEPS {
        std::thread::sleep(STREAM_STEP);
        let delta = if step % 2 == 0 {
            serde_json::json!({"type":"input_json_delta","partial_json":"{"})
        } else {
            serde_json::json!({"type":"thinking_delta","thinking":"hm"})
        };
        emit(
            out,
            &serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta","delta":delta}}),
        )?;
    }
    emit(
        out,
        &serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"ran"}]}}),
    )?;
    emit(out, &serde_json::json!({"type":"result","is_error":false}))
}

/// A tool Claude Code runs without asking, reported at once as done or as failed.
fn run_quick_tool(is_error: bool, out: &mut impl Write) -> io::Result<()> {
    emit(
        out,
        &serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":TOOL_USE_ID,"name":"Bash","input":{"command":"ls"}}]}}),
    )?;
    emit(
        out,
        &serde_json::json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":TOOL_USE_ID,"content":"x","is_error":is_error}]}}),
    )?;
    emit(out, &serde_json::json!({"type":"result","is_error":false}))
}

/// A tool Claude Code runs without asking: announced, silent, then reported.
#[expect(
    clippy::disallowed_methods,
    reason = "the fake provider simulates provider latency in real time"
)]
fn run_unasked_tool(user_line: &str, out: &mut impl Write) -> io::Result<()> {
    emit(
        out,
        &serde_json::json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","id":TOOL_USE_ID,"name":"Bash","input":{"command":"ls"}}]}}),
    )?;
    if user_line.contains(TOOL_HANG_TRIGGER) {
        park();
    }
    std::thread::sleep(TOOL_SLOW);
    emit(
        out,
        &serde_json::json!({"type":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":TOOL_USE_ID,"content":"ok"}]}}),
    )?;
    emit(
        out,
        &serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"ran"}]}}),
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

#[expect(
    clippy::disallowed_methods,
    reason = "the fake provider simulates provider latency in real time"
)]
fn main() -> io::Result<()> {
    if std::env::args().any(|arg| arg == "--version") {
        println!("{VERSION}");
        return Ok(());
    }
    let control = Control::load();
    if let Some(path) = control.path("report_path") {
        write_report(path)?;
    }
    if let Some(path) = control.path("start_log") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{}", std::process::id())?;
    }
    if (control.rejects_resume() && std::env::args().any(|arg| arg == RESUME_FLAG))
        || control.rejects_this_resume()
    {
        let mut prompt = String::new();
        io::stdin().lock().read_line(&mut prompt)?;
        std::process::exit(CRASH_EXIT_CODE);
    }
    if let Some(marker) = control.path("resume_crash_marker")
        && std::env::args().any(|arg| arg == RESUME_FLAG)
        && !marker.exists()
    {
        std::fs::write(marker, b"crashed")?;
        let mut prompt = String::new();
        io::stdin().lock().read_line(&mut prompt)?;
        std::process::exit(CRASH_EXIT_CODE);
    }
    if let Some(gate) = control.path("start_gate") {
        let deadline = Instant::now() + GATE_LIMIT;
        while !gate.exists() {
            if Instant::now() >= deadline {
                eprintln!("fake-claude: the start gate never opened");
                std::process::exit(1);
            }
            std::thread::sleep(GATE_POLL);
        }
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
    let mut init = serde_json::json!({"type":"system","subtype":"init","session_id":SESSION_ID});
    if let Some(source) = control.text("api_key_source") {
        init["apiKeySource"] = serde_json::json!(source);
    }
    if let (Some(fields), Some(extra)) = (init.as_object_mut(), control.0["init"].as_object()) {
        fields.extend(extra.clone());
    }
    emit(&mut out, &init)?;
    let mut lines = io::stdin().lock().lines();
    while let Some(line) = lines.next().transpose()? {
        if !line.trim().is_empty() {
            run_turn(&line, &mut lines, &mut out, &control)?;
        }
    }
    Ok(())
}
