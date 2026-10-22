// SPDX-License-Identifier: AGPL-3.0-only

//! Records raw transcripts from the real `claude` and `codex` CLIs, run through
//! coda's own adapters. Spends subscription quota, so every test is `#[ignore]`
//! and also needs `CODA_LIVE_TESTS=1`; CI never runs it.
//!
//! ```sh
//! mkdir /tmp/cdlive   # keep it short: the app-server socket path has a limit
//! CODA_LIVE_TESTS=1 CODA_LIVE_OUT=/tmp/cdlive \
//!   cargo nextest run -p coda-providers --test live_record --run-ignored only
//! ```
//!
//! Each test writes `<name>.raw.jsonl` into `CODA_LIVE_OUT`. Raw files hold paths
//! and ids: run them through `coda-selfhost scrub` before committing anything
//! (see `docs/live-recording.md`).
//!
//! The CLIs are reached through a shell shim that copies their traffic to files
//! while the adapter drives them unchanged. The Claude shim adds containment
//! flags (cheap model, two turns, no user settings, hooks or MCP servers); the
//! Codex shims add nothing. The app-server speaks WebSocket on a Unix socket, so
//! its shim moves the socket and a proxy in this test logs the text frames.
#![cfg(unix)]

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coda_protocol::events::Event;
use coda_protocol::types::{PermissionDecision, SessionId, TurnId};
use coda_providers::claude_session::{CancelHandle, ClaudeSession, SessionSpec, TurnOutcome};
use coda_providers::codex_app_server::{AppServerSpec, CodexAppServerSession};
use coda_providers::codex_exec::{CodexExecSession, CodexSpec, SandboxMode};
use coda_providers::gate::PermissionGate;
use coda_providers::tool_request::ToolRequest;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream, unix::OwnedReadHalf, unix::OwnedWriteHalf};

const CLAUDE_TURN_BOUND: Duration = Duration::from_secs(120);
const CODEX_TURN_BOUND: Duration = Duration::from_secs(180);
const PROXY_CONNECT_BOUND: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);
/// How long the long command runs before the cancelled-turn recording interrupts it.
const CANCEL_AFTER_APPROVAL: Duration = Duration::from_secs(3);
const AUTH_FILE: &str = "auth.json";
const RECORDER: &str = "coda-providers/tests/live_record.rs";
const RECORDED_AT_FORMAT: &str = "+%Y-%m-%dT%H:%M:%SZ";

const CLAUDE_ALLOW_PROMPT: &str =
    "Run the shell command `touch hello.txt` with the Bash tool, then reply with the word done.";
const CLAUDE_READ_ONLY_PROMPT: &str =
    "Run the shell command `echo hello` with the Bash tool, then reply with the word done.";
const CLAUDE_TEXT_PROMPT: &str = "Reply with the single word: ok";
const CODEX_FIRST_PROMPT: &str = "Reply with the single word: ok";
const CODEX_SECOND_PROMPT: &str = "Reply with the single word: done";
const CODEX_APPROVAL_PROMPT: &str = "Run the shell command `touch hello.txt`. The sandbox is \
    read-only, so do not try it inside the sandbox first: request escalated permissions \
    (sandbox_permissions require_escalated) with a one line justification, and wait for approval.";
const CODEX_LONG_COMMAND_PROMPT: &str = "Run the shell command `sleep 30`. The sandbox is \
    read-only, so do not try it inside the sandbox first: request escalated permissions \
    (sandbox_permissions require_escalated) with a one line justification, and wait for approval.";

/// A throwaway `CODEX_HOME` that holds only the login, so none of the user's
/// MCP servers, plugins or skills load. The login copy is deleted on drop.
struct CodexHome {
    dir: PathBuf,
}

impl CodexHome {
    fn create(out: &Path, name: &str) -> Self {
        let dir = out.join(format!("{name}.home"));
        fs::create_dir_all(&dir).unwrap();
        let user_home = std::env::var_os("CODEX_HOME").map_or_else(
            || PathBuf::from(std::env::var_os("HOME").expect("HOME is not set")).join(".codex"),
            PathBuf::from,
        );
        fs::copy(user_home.join(AUTH_FILE), dir.join(AUTH_FILE))
            .expect("could not copy the Codex login");
        fs::write(dir.join("config.toml"), "").unwrap();
        Self { dir }
    }
}

impl Drop for CodexHome {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.dir.join(AUTH_FILE));
    }
}

struct FixedGate {
    decision: PermissionDecision,
    seen: Mutex<Vec<ToolRequest>>,
    /// When set, the turn is cancelled this long after the first request is answered.
    cancel: Option<(CancelHandle, Duration)>,
}

impl FixedGate {
    fn new(decision: PermissionDecision) -> Self {
        Self {
            decision,
            seen: Mutex::new(Vec::new()),
            cancel: None,
        }
    }

    fn cancelling(mut self, handle: CancelHandle, after: Duration) -> Self {
        self.cancel = Some((handle, after));
        self
    }
}

impl PermissionGate for FixedGate {
    fn request(&self, request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        self.seen.lock().unwrap().push(request.clone());
        if let Some((handle, after)) = self.cancel.clone() {
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                handle.cancel();
            });
        }
        std::future::ready(self.decision)
    }
}

/// The output directory, or `None` when live recording is not switched on.
fn live_out() -> Option<PathBuf> {
    if std::env::var("CODA_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set CODA_LIVE_TESTS=1 to record live transcripts");
        return None;
    }
    let out = std::env::var_os("CODA_LIVE_OUT").expect("set CODA_LIVE_OUT to a short directory");
    Some(PathBuf::from(out))
}

fn find_binary(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("`{name}` is not on PATH"))
}

fn command_text(binary: &Path, args: &[&str]) -> String {
    let output = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("could not run the CLI");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// `2.1.296 (Claude Code)` becomes `2.1.296`.
fn claude_version(binary: &Path) -> String {
    let text = command_text(binary, &["--version"]);
    text.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// `codex-cli 0.163.0` becomes `0.163.0`.
fn codex_version(binary: &Path) -> String {
    let text = command_text(binary, &["--version"]);
    text.split_whitespace()
        .last()
        .unwrap_or_default()
        .to_owned()
}

fn now() -> String {
    command_text(Path::new("date"), &["-u", RECORDED_AT_FORMAT])
}

fn work_dir(out: &Path) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("w")
        .tempdir_in(out)
        .expect("could not create the empty working directory")
}

/// `codex exec` refuses to run outside a Git repository unless told otherwise.
fn git_work_dir(out: &Path) -> tempfile::TempDir {
    let dir = work_dir(out);
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .status()
        .expect("could not run git");
    assert!(status.success(), "git init failed");
    dir
}

fn write_shim(out: &Path, name: &str, script: &str) -> PathBuf {
    let path = out.join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn path_str(path: &Path) -> &str {
    path.to_str().expect("temp paths are UTF-8")
}

fn parent_env() -> Vec<(OsString, OsString)> {
    std::env::vars_os().collect()
}

fn lines_of(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn write_raw(out: &Path, name: &str, header: &Value, lines: &[String]) {
    let mut text = header.to_string();
    for line in lines {
        text.push('\n');
        text.push_str(line);
    }
    text.push('\n');
    fs::write(out.join(format!("{name}.raw.jsonl")), text).unwrap();
}

/// What the adapter made of the run, for the discrepancy report; not a fixture.
fn write_adapter_notes(out: &Path, name: &str, notes: &str) {
    fs::write(out.join(format!("{name}.adapter.txt")), notes).unwrap();
}

fn request_id_of(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line).ok()?;
    match value["type"].as_str()? {
        "control_request" => value["request_id"].as_str().map(str::to_owned),
        "control_response" => value["response"]["request_id"].as_str().map(str::to_owned),
        _ => None,
    }
}

/// Keeps what Claude Code wrote and puts each answer coda sent right after the
/// request it answers. The prompt coda sent is in the header, not the stream.
fn merge_claude(sent: &[String], received: &[String]) -> Vec<String> {
    let mut merged = Vec::new();
    for line in received {
        merged.push(line.clone());
        let is_request = serde_json::from_str::<Value>(line)
            .is_ok_and(|value| value["type"] == "control_request");
        if let (true, Some(id)) = (is_request, request_id_of(line)) {
            merged.extend(
                sent.iter()
                    .filter(|l| request_id_of(l).as_deref() == Some(id.as_str()))
                    .cloned(),
            );
        }
    }
    merged
}

fn claude_shim(out: &Path, name: &str, real: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let sent = out.join(format!("{name}.sent"));
    let received = out.join(format!("{name}.received"));
    // The shim appends, so a rerun must start from empty files.
    let _ = (fs::remove_file(&sent), fs::remove_file(&received));
    let script = format!(
        "#!/bin/sh\n/usr/bin/tee -a '{}' | '{}' \"$@\" --model haiku --max-turns 2 \
         --setting-sources project --strict-mcp-config --disable-slash-commands \
         | /usr/bin/tee -a '{}'\n",
        path_str(&sent),
        path_str(real),
        path_str(&received),
    );
    (
        write_shim(out, &format!("{name}.sh"), &script),
        sent,
        received,
    )
}

async fn record_claude(
    out: &Path,
    name: &str,
    prompt: &str,
    decision: PermissionDecision,
) -> Vec<String> {
    let real = find_binary("claude");
    let (shim, sent, received) = claude_shim(out, name, &real);
    let cwd = work_dir(out);
    let mut session = ClaudeSession::spawn(SessionSpec {
        binary: shim,
        cwd: cwd.path().to_path_buf(),
        session_id: SessionId::new("live"),
        resume: None,
        parent_env: parent_env(),
    })
    .expect("could not start the shim");
    let gate = FixedGate::new(decision);
    let mut events: Vec<Event> = Vec::new();
    let turn = TurnId::new("t-1");
    let outcome = tokio::time::timeout(
        CLAUDE_TURN_BOUND,
        session.run_turn(&turn, prompt, &gate, &mut |event| events.push(event)),
    )
    .await;
    let diagnostics = format!("{:?}", session.diagnostics());
    session.shutdown().await.unwrap();

    let merged = merge_claude(&lines_of(&sent), &lines_of(&received));
    let header = json!({
        "type": "coda_recording", "label": "live",
        "claude_version": claude_version(&real),
        "recorded_at": now(), "recorder": RECORDER,
        "scenario": name, "prompt": prompt,
        "note": "recorded through ClaudeSession; the shim added --model haiku --max-turns 2 \
                 --setting-sources project --strict-mcp-config --disable-slash-commands",
    });
    write_raw(out, name, &header, &merged);
    write_adapter_notes(
        out,
        name,
        &format!(
            "outcome: {outcome:?}\nevents: {events:#?}\ndiagnostics: {diagnostics}\n\
             requests the gate saw: {:#?}\n",
            gate.seen.lock().unwrap()
        ),
    );
    assert!(
        matches!(outcome, Ok(Ok(TurnOutcome::Completed { is_error: false }))),
        "the turn did not complete: {outcome:?}"
    );
    merged
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn claude_permission_request_answered_allow() {
    let Some(out) = live_out() else { return };
    record_claude(
        &out,
        "claude-live-allow",
        CLAUDE_ALLOW_PROMPT,
        PermissionDecision::AllowOnce,
    )
    .await;
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn claude_permission_request_answered_deny() {
    let Some(out) = live_out() else { return };
    record_claude(
        &out,
        "claude-live-deny",
        CLAUDE_ALLOW_PROMPT,
        PermissionDecision::Deny,
    )
    .await;
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn claude_read_only_command_raises_no_permission_request() {
    let Some(out) = live_out() else { return };
    let lines = record_claude(
        &out,
        "claude-live-readonly",
        CLAUDE_READ_ONLY_PROMPT,
        PermissionDecision::Deny,
    )
    .await;
    assert!(
        lines.iter().all(|line| request_id_of(line).is_none()),
        "Claude Code asked about a read-only command"
    );
}

#[ignore = "spends Claude subscription quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn claude_plain_text_turn() {
    let Some(out) = live_out() else { return };
    record_claude(
        &out,
        "claude-live-text",
        CLAUDE_TEXT_PROMPT,
        PermissionDecision::Deny,
    )
    .await;
}

#[ignore = "spends Codex quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn codex_exec_plain_turn_then_resume() {
    let Some(out) = live_out() else { return };
    let real = find_binary("codex");
    let sent = out.join("codex-exec.sent");
    let received = out.join("codex-exec.received");
    let _ = (fs::remove_file(&sent), fs::remove_file(&received));
    let home = CodexHome::create(&out, "codex-exec");
    let script = format!(
        "#!/bin/sh\nexport CODEX_HOME='{}'\n/usr/bin/tee -a '{}' | '{}' \"$@\" | /usr/bin/tee -a '{}'\n",
        path_str(&home.dir),
        path_str(&sent),
        path_str(&real),
        path_str(&received),
    );
    let cwd = git_work_dir(&out);
    let mut session = CodexExecSession::new(CodexSpec {
        binary: write_shim(&out, "codex-exec.sh", &script),
        cwd: cwd.path().to_path_buf(),
        session_id: SessionId::new("live"),
        resume: None,
        sandbox: SandboxMode::ReadOnly,
        parent_env: parent_env(),
    });
    let mut notes = String::new();
    let mut outcomes = Vec::new();
    for (index, prompt) in [CODEX_FIRST_PROMPT, CODEX_SECOND_PROMPT].iter().enumerate() {
        let mut events: Vec<Event> = Vec::new();
        let turn = TurnId::new(format!("t-{index}"));
        let outcome = tokio::time::timeout(
            CODEX_TURN_BOUND,
            session.run_turn(
                &turn,
                prompt,
                &FixedGate::new(PermissionDecision::Deny),
                &mut |event| events.push(event),
            ),
        )
        .await;
        write!(
            notes,
            "turn {index}: {outcome:?}\nevents: {events:#?}\ndiagnostics: {:?}\n",
            session.diagnostics()
        )
        .unwrap();
        outcomes.push(outcome);
        if !matches!(
            outcomes[index],
            Ok(Ok(TurnOutcome::Completed { is_error: false }))
        ) {
            break;
        }
    }
    session.shutdown().await.unwrap();
    let header = json!({
        "type": "coda_recording", "label": "live",
        "codex_version": codex_version(&real),
        "recorded_at": now(), "recorder": RECORDER,
        "scenario": "exec turn, then a second turn that resumes the thread",
        "prompts": [CODEX_FIRST_PROMPT, CODEX_SECOND_PROMPT],
        "note": "recorded through CodexExecSession with argv `exec --json --sandbox read-only \
                 [resume <thread>] -`; lines are the stdout of each process in order",
    });
    write_raw(&out, "codex-exec-live", &header, &lines_of(&received));
    write_adapter_notes(&out, "codex-exec-live", &notes);
    assert!(
        outcomes.len() == 2
            && outcomes
                .iter()
                .all(|o| matches!(o, Ok(Ok(TurnOutcome::Completed { is_error: false })))),
        "both turns must complete: {outcomes:?}"
    );
}

/// Splits a WebSocket byte stream into text frames, logging each as it completes.
struct FrameLog {
    dir: &'static str,
    head_done: bool,
    buffer: Vec<u8>,
}

impl FrameLog {
    fn new(dir: &'static str) -> Self {
        Self {
            dir,
            head_done: false,
            buffer: Vec::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8], log: &Mutex<Vec<Value>>) {
        self.buffer.extend_from_slice(bytes);
        if !self.head_done {
            let Some(end) = self.buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                return;
            };
            let head = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
            let status_line = head.lines().next().unwrap_or_default();
            log.lock()
                .unwrap()
                .push(json!({"dir": self.dir, "http": status_line}));
            self.buffer.drain(..end + 4);
            self.head_done = true;
        }
        while let Some((opcode, payload, used)) = next_frame(&self.buffer) {
            if opcode == 1
                && let Ok(message) = serde_json::from_slice::<Value>(&payload)
            {
                log.lock()
                    .unwrap()
                    .push(json!({"dir": self.dir, "message": message}));
            }
            self.buffer.drain(..used);
        }
    }
}

/// The first complete frame in `buffer`: opcode, unmasked payload, bytes used.
/// Fragmented messages are not reassembled; the app-server does not send them.
fn next_frame(buffer: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    let (first, second) = (*buffer.first()?, *buffer.get(1)?);
    let masked = second & 0x80 != 0;
    let (len, mut at) = match second & 0x7f {
        126 => (
            usize::from(u16::from_be_bytes(buffer.get(2..4)?.try_into().ok()?)),
            4,
        ),
        127 => (
            usize::try_from(u64::from_be_bytes(buffer.get(2..10)?.try_into().ok()?)).ok()?,
            10,
        ),
        short => (usize::from(short), 2),
    };
    let mask: [u8; 4] = if masked {
        let mask = buffer.get(at..at + 4)?.try_into().ok()?;
        at += 4;
        mask
    } else {
        [0; 4]
    };
    let payload = buffer.get(at..at + len)?;
    let payload = payload
        .iter()
        .enumerate()
        .map(|(i, byte)| if masked { byte ^ mask[i % 4] } else { *byte })
        .collect();
    Some((first & 0x0f, payload, at + len))
}

async fn pump(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    mut frames: FrameLog,
    log: Arc<Mutex<Vec<Value>>>,
) {
    let mut chunk = vec![0_u8; 16 * 1024];
    while let Ok(read) = from.read(&mut chunk).await {
        if read == 0 {
            break;
        }
        frames.feed(&chunk[..read], &log);
        if to.write_all(&chunk[..read]).await.is_err() {
            break;
        }
    }
}

/// Listens where the adapter expects the server, and forwards to the real one.
async fn proxy(listen_file: PathBuf, log: Arc<Mutex<Vec<Value>>>) {
    let socket = loop {
        match fs::read_to_string(&listen_file) {
            Ok(path) if !path.is_empty() => break PathBuf::from(path),
            _ => tokio::time::sleep(POLL).await,
        }
    };
    let listener = UnixListener::bind(&socket).expect("could not bind the proxy socket");
    let (client, _) = listener
        .accept()
        .await
        .expect("the adapter never connected");
    let upstream_path = PathBuf::from(format!("{}.real", socket.display()));
    let deadline = tokio::time::Instant::now() + PROXY_CONNECT_BOUND;
    let server = loop {
        match UnixStream::connect(&upstream_path).await {
            Ok(stream) => break stream,
            Err(error) if tokio::time::Instant::now() >= deadline => {
                panic!("the real server never listened: {error}")
            }
            Err(_) => tokio::time::sleep(POLL).await,
        }
    };
    let (client_read, client_write) = client.into_split();
    let (server_read, server_write) = server.into_split();
    let up = pump(
        client_read,
        server_write,
        FrameLog::new("send"),
        Arc::clone(&log),
    );
    let down = pump(server_read, client_write, FrameLog::new("recv"), log);
    tokio::join!(up, down);
}

/// What one app-server recording asks Codex to do and how coda answers.
struct AppServerRun {
    prompt: &'static str,
    decision: PermissionDecision,
    /// Cancel the turn once the approval is answered, so the adapter interrupts it.
    cancel: bool,
}

async fn record_app_server(out: &Path, name: &str, run: AppServerRun) {
    let real = find_binary("codex");
    let listen_file = out.join(format!("{name}.listen"));
    let _ = fs::remove_file(&listen_file);
    let home = CodexHome::create(out, name);
    let script = format!(
        "#!/bin/sh\nexport CODEX_HOME='{2}'\nsocket=\"${{3#unix://}}\"\nprintf '%s' \"$socket\" > '{0}.tmp' && mv '{0}.tmp' '{0}'\n\
         exec '{1}' \"$1\" \"$2\" \"unix://$socket.real\"\n",
        path_str(&listen_file),
        path_str(&real),
        path_str(&home.dir),
    );
    let shim = write_shim(out, &format!("{name}.sh"), &script);
    let log = Arc::new(Mutex::new(Vec::new()));
    let proxy_task = tokio::spawn(proxy(listen_file, Arc::clone(&log)));

    let cwd = work_dir(out);
    let started = tokio::time::timeout(
        CODEX_TURN_BOUND,
        CodexAppServerSession::start(
            AppServerSpec {
                binary: shim,
                cwd: cwd.path().to_path_buf(),
                session_id: SessionId::new("live"),
                sandbox: SandboxMode::ReadOnly,
                ephemeral: true,
                verified_versions: Vec::new(),
                parent_env: parent_env(),
            },
            None,
        ),
    )
    .await;
    let mut notes = String::new();
    let mut outcome = None;
    let mut approvals_seen = 0;
    if let Ok(Ok(mut session)) = started {
        let mut gate = FixedGate::new(run.decision);
        if run.cancel {
            gate = gate.cancelling(session.cancel_handle(), CANCEL_AFTER_APPROVAL);
        }
        let mut events: Vec<Event> = Vec::new();
        let turn = TurnId::new("t-1");
        let result = tokio::time::timeout(
            CODEX_TURN_BOUND,
            session.run_turn(&turn, run.prompt, &gate, &mut |e| events.push(e)),
        )
        .await;
        notes = format!(
            "server version: {:?}\ndrift: {:?}\noutcome: {result:?}\nevents: {events:#?}\n\
             requests the gate saw: {:#?}\n",
            session.server_version(),
            session.drift_warning(),
            gate.seen.lock().unwrap()
        );
        outcome = Some(result);
        approvals_seen = gate.seen.lock().unwrap().len();
        session.shutdown().await.unwrap();
    } else {
        write!(notes, "start failed: {:?}", started.map(|r| r.map(|_| ()))).unwrap();
    }
    proxy_task.abort();

    let lines: Vec<String> = log.lock().unwrap().iter().map(Value::to_string).collect();
    let header = json!({
        "type": "coda_recording", "label": "live",
        "codex_version": codex_version(&real),
        "recorded_at": now(), "recorder": RECORDER,
        "scenario": name, "prompt": run.prompt,
        "note": "recorded through CodexAppServerSession behind a logging proxy; each line is \
                 {dir: send|recv, message} for a JSON-RPC text frame, or {dir, http} for the upgrade",
    });
    write_raw(out, name, &header, &lines);
    write_adapter_notes(out, name, &notes);
    let ended_as_expected = if run.cancel {
        matches!(outcome, Some(Ok(Ok(TurnOutcome::Cancelled))))
    } else {
        matches!(outcome, Some(Ok(Ok(TurnOutcome::Completed { .. }))))
    };
    assert!(
        ended_as_expected,
        "the turn did not end as the scenario expects; see {name}.adapter.txt"
    );
    assert!(
        approvals_seen > 0,
        "Codex never asked for approval, so this is not an approval transcript; see {name}.adapter.txt"
    );
}

#[ignore = "spends Codex quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn codex_app_server_approval_accepted() {
    let Some(out) = live_out() else { return };
    let run = AppServerRun {
        prompt: CODEX_APPROVAL_PROMPT,
        decision: PermissionDecision::AllowOnce,
        cancel: false,
    };
    record_app_server(&out, "codex-app-live-accept", run).await;
}

#[ignore = "spends Codex quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn codex_app_server_approval_declined() {
    let Some(out) = live_out() else { return };
    let run = AppServerRun {
        prompt: CODEX_APPROVAL_PROMPT,
        decision: PermissionDecision::Deny,
        cancel: false,
    };
    record_app_server(&out, "codex-app-live-decline", run).await;
}

#[ignore = "spends Codex quota; needs CODA_LIVE_TESTS=1"]
#[tokio::test]
async fn codex_app_server_turn_cancelled_during_a_long_command() {
    let Some(out) = live_out() else { return };
    let run = AppServerRun {
        prompt: CODEX_LONG_COMMAND_PROMPT,
        decision: PermissionDecision::AllowOnce,
        cancel: true,
    };
    record_app_server(&out, "codex-app-live-cancel", run).await;
}
