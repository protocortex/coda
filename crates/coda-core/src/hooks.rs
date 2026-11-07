// SPDX-License-Identifier: Apache-2.0

//! Hooks: user commands run at session start and around tool calls.
//!
//! A hook is a program and its arguments, never a shell string. It runs in the
//! session's directory with the environment it was given and nothing else, gets
//! a JSON payload on standard input, and is killed with its whole process group
//! when it outlives its timeout or the runner stops waiting for it. Only a hook
//! marked `blocking` can stop the action it guards; a failing hook that is not
//! blocking is logged and the action goes on.

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use coda_config::hooks::{HookConfig, HookEvent};
use coda_protocol::types::SessionId;
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::task::JoinHandle;
use tracing::warn;

/// The most bytes of a call's arguments, as JSON text, that a hook gets whole.
/// A call over it is cut for a hook that is not blocking and stops a blocking
/// one, because a guard that sees part of a call can be walked around. The
/// bytes are counted before the text is escaped again inside the payload.
pub const MAX_ARGUMENTS_BYTES: usize = 256 * 1024;
/// The most bytes of a call's result, as the model will see it, that a hook
/// gets whole. It is above what the daemon compacts a result to (16 KiB and a
/// footer), so a guard sees all that reaches the model. Over it, as for
/// [`MAX_ARGUMENTS_BYTES`]. The bytes are counted before JSON escaping.
pub const MAX_RESULT_BYTES: usize = 64 * 1024;
/// The exit status with which an `end_turn` hook asks for another try. Any other
/// failure, a crash or a timeout included, lets the turn end.
pub const GUARD_RETRY_EXIT_CODE: i32 = 2;
/// Told to the model when an `end_turn` hook asks for another try and says nothing.
pub const GUARD_DEFAULT_INSTRUCTION: &str =
    "an end_turn hook asked you to keep working, and gave no reason";
/// The most bytes of a hook's standard error kept as the reason it failed.
pub const MAX_REASON_BYTES: usize = 1024;
/// The reason a blocking hook stops a call it could not be given whole: `what`
/// names the part of the call that went over its cap.
fn too_large(what: &str, cap: usize) -> String {
    format!("{what} too large for the hook to check (over {cap} bytes)")
}
/// How long to wait for the end of a failed hook's standard error once the hook
/// is gone, in case a process it left behind still holds the pipe.
const READER_GRACE: Duration = Duration::from_millis(500);
const READ_CHUNK_BYTES: usize = 4 * 1024;
/// Names a hook that has no program, in a block message or a log line.
const NO_PROGRAM: &str = "(no program)";
/// The variable a bare program name is looked up in.
const PATH_VAR: &str = "PATH";
const ESCAPE: char = '\u{1b}';
const CSI_START: char = '[';
/// The bytes that end a control sequence.
const CSI_FINAL: std::ops::RangeInclusive<char> = '@'..='~';
/// Unicode line and paragraph separators, which start a new line without being control characters.
const LINE_SEPARATORS: [char; 2] = ['\u{2028}', '\u{2029}'];
/// A file any of whose permission bits is set in this mask can be run.
const EXECUTE_BITS: u32 = 0o111;
/// Unicode format characters (category Cf) that hide text or reorder it:
/// zero width and bidirectional controls, the byte order mark, tag characters.
const FORMAT_RANGES: [(char, char); 20] = [
    ('\u{00AD}', '\u{00AD}'),
    ('\u{0600}', '\u{0605}'),
    ('\u{061C}', '\u{061C}'),
    ('\u{06DD}', '\u{06DD}'),
    ('\u{070F}', '\u{070F}'),
    ('\u{0890}', '\u{0891}'),
    ('\u{08E2}', '\u{08E2}'),
    ('\u{180E}', '\u{180E}'),
    ('\u{200B}', '\u{200F}'),
    ('\u{202A}', '\u{202E}'),
    ('\u{2060}', '\u{206F}'),
    ('\u{FEFF}', '\u{FEFF}'),
    ('\u{FFF9}', '\u{FFFB}'),
    ('\u{110BD}', '\u{110BD}'),
    ('\u{110CD}', '\u{110CD}'),
    ('\u{13430}', '\u{1343F}'),
    ('\u{1BCA0}', '\u{1BCA3}'),
    ('\u{1D173}', '\u{1D17A}'),
    ('\u{E0001}', '\u{E0001}'),
    ('\u{E0020}', '\u{E007F}'),
];
/// A UTF-8 continuation byte is `10xxxxxx`.
const CONTINUATION_MASK: u8 = 0b1100_0000;
const CONTINUATION_TAG: u8 = 0b1000_0000;

/// A blocking hook failed, so the action it guards does not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("blocked by the {event} hook `{hook}`: {reason}")]
pub struct Blocked {
    pub event: HookEvent,
    /// The hook's place in the config and its program, as `hooks[2] (/bin/sh)`.
    pub hook: String,
    /// How the hook ended, then the end of its standard error, on one line with
    /// no control or format characters.
    pub reason: String,
}

/// How a hook failed.
enum Failure {
    /// The entry has no program to run; config validation refuses it, but a
    /// runner can be built from entries that skipped validation.
    EmptyCommand,
    Start(io::Error),
    Wait(io::Error),
    Exited(ExitStatus),
    TimedOut(Duration),
}

impl Failure {
    fn describe(&self) -> String {
        match self {
            Self::EmptyCommand => "has an empty command".to_owned(),
            Self::Start(error) => format!("could not start: {error}"),
            Self::Wait(error) => format!("could not be waited for: {error}"),
            Self::Exited(status) => match status.code() {
                Some(code) => format!("exited with status {code}"),
                None => "was ended by a signal".to_owned(),
            },
            Self::TimedOut(limit) => format!("timed out after {} ms", limit.as_millis()),
        }
    }
}

/// A hook and its place in the config, which names it in a message.
#[derive(Debug, Clone)]
struct Entry {
    index: usize,
    config: HookConfig,
}

impl Entry {
    fn name(&self) -> String {
        let program = self
            .config
            .command
            .first()
            .map_or(NO_PROGRAM, String::as_str);
        format!("hooks[{}] ({program})", self.index)
    }
}

/// What a hook is given on standard input.
struct Payload {
    text: String,
    /// Why a blocking hook cannot be given this payload, when the call was too
    /// large and `text` holds only a cut of it.
    too_large: Option<String>,
}

/// The hooks of a session and the environment they run with.
#[derive(Debug, Clone, Default)]
pub struct HookRunner {
    hooks: Vec<Entry>,
    env: Vec<(OsString, OsString)>,
}

impl HookRunner {
    /// A runner for `hooks`. Their processes see `env` and nothing else.
    #[must_use]
    pub fn new(hooks: &[HookConfig], env: Vec<(OsString, OsString)>) -> Self {
        let hooks = hooks
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, config)| Entry { index, config })
            .collect();
        Self { hooks, env }
    }

    /// This runner without its hooks for `event`. The others keep the place
    /// they have in the config.
    #[must_use]
    pub fn without(mut self, event: HookEvent) -> Self {
        self.hooks.retain(|entry| entry.config.event != event);
        self
    }

    /// Whether any hook is set for `event`.
    #[must_use]
    pub fn has(&self, event: HookEvent) -> bool {
        self.hooks.iter().any(|entry| entry.config.event == event)
    }

    /// Whether a hook set for `event` can stop the action it guards.
    #[must_use]
    pub fn has_blocking(&self, event: HookEvent) -> bool {
        (self.hooks.iter()).any(|entry| entry.config.event == event && entry.config.blocking)
    }

    /// Runs the `session_start` hooks. `resumed` tells them a `session_start`
    /// hook already passed for this session.
    ///
    /// # Errors
    /// Returns [`Blocked`] when a blocking hook fails.
    pub async fn session_start(
        &self,
        session: &SessionId,
        cwd: &Path,
        resumed: bool,
    ) -> Result<(), Blocked> {
        if !self.has(HookEvent::SessionStart) {
            return Ok(());
        }
        let payload = json!({
            "event": HookEvent::SessionStart.as_str(),
            "session": session,
            "cwd": cwd.to_string_lossy(),
            "resumed": resumed,
        });
        let payload = Payload {
            text: payload.to_string(),
            too_large: None,
        };
        self.run(HookEvent::SessionStart, cwd, &payload).await
    }

    /// Runs the `pre_tool` hooks for a call to `tool` with `arguments`.
    ///
    /// # Errors
    /// Returns [`Blocked`] when a blocking hook fails.
    pub async fn pre_tool(
        &self,
        session: &SessionId,
        cwd: &Path,
        tool: &str,
        arguments: &Value,
    ) -> Result<(), Blocked> {
        if !self.has(HookEvent::PreTool) {
            return Ok(());
        }
        let mut payload = json!({
            "event": HookEvent::PreTool.as_str(),
            "session": session,
            "cwd": cwd.to_string_lossy(),
            "tool": tool,
        });
        let text = arguments.to_string();
        let cut = text.len() > MAX_ARGUMENTS_BYTES;
        if cut {
            // The head of the JSON text, which is no longer valid JSON.
            payload["arguments"] = json!(head(&text, MAX_ARGUMENTS_BYTES));
            payload["arguments_truncated"] = json!(true);
        } else {
            payload["arguments"] = arguments.clone();
        }
        let payload = Payload {
            text: payload.to_string(),
            too_large: cut.then(|| too_large("the arguments are", MAX_ARGUMENTS_BYTES)),
        };
        self.run(HookEvent::PreTool, cwd, &payload).await
    }

    /// Runs the `post_tool` hooks for a call to `tool` that returned `result`.
    ///
    /// # Errors
    /// Returns [`Blocked`] when a blocking hook fails.
    pub async fn post_tool(
        &self,
        session: &SessionId,
        cwd: &Path,
        tool: &str,
        is_error: bool,
        result: &str,
    ) -> Result<(), Blocked> {
        if !self.has(HookEvent::PostTool) {
            return Ok(());
        }
        let mut payload = json!({
            "event": HookEvent::PostTool.as_str(),
            "session": session,
            "cwd": cwd.to_string_lossy(),
            "tool": tool,
            "is_error": is_error,
            "result": head(result, MAX_RESULT_BYTES),
        });
        let cut = result.len() > MAX_RESULT_BYTES;
        if cut {
            payload["result_truncated"] = json!(true);
        }
        let payload = Payload {
            text: payload.to_string(),
            too_large: cut.then(|| too_large("the result is", MAX_RESULT_BYTES)),
        };
        self.run(HookEvent::PostTool, cwd, &payload).await
    }

    /// Runs the `end_turn` hooks for a reply of `text` that came after
    /// `continues` forced continues. Returns what to tell the model when a
    /// blocking hook exits with [`GUARD_RETRY_EXIT_CODE`], from its standard
    /// error. A hook that is not blocking, or that fails any other way, never
    /// keeps the turn going: it is logged.
    pub async fn end_turn(
        &self,
        session: &SessionId,
        cwd: &Path,
        text: &str,
        continues: u32,
    ) -> Option<String> {
        if !self.has(HookEvent::EndTurn) {
            return None;
        }
        let mut payload = json!({
            "event": HookEvent::EndTurn.as_str(),
            "session": session,
            "cwd": cwd.to_string_lossy(),
            "text": head(text, MAX_RESULT_BYTES),
            "continues": continues,
        });
        if text.len() > MAX_RESULT_BYTES {
            payload["text_truncated"] = json!(true);
        }
        let payload = payload.to_string();
        for entry in self
            .hooks
            .iter()
            .filter(|entry| entry.config.event == HookEvent::EndTurn)
        {
            let Err((failure, tail)) = self.execute(&entry.config, payload.as_bytes(), cwd).await
            else {
                continue;
            };
            let asked = matches!(&failure, Failure::Exited(status) if status.code() == Some(GUARD_RETRY_EXIT_CODE));
            if asked && entry.config.blocking {
                let said = plain_lines(&tail);
                let said = said.trim();
                return Some(if said.is_empty() {
                    GUARD_DEFAULT_INSTRUCTION.to_owned()
                } else {
                    said.to_owned()
                });
            }
            warn!(
                "hook failed, continuing: event={} hook={} reason={}",
                HookEvent::EndTurn,
                entry.name(),
                one_line(&failure.describe())
            );
        }
        None
    }

    /// Runs the hooks of `event` in order. The first blocking hook that fails
    /// ends the run; one that is not blocking is logged. A blocking hook that
    /// would get only part of the call does not run and stops it.
    async fn run(&self, event: HookEvent, cwd: &Path, payload: &Payload) -> Result<(), Blocked> {
        for entry in self
            .hooks
            .iter()
            .filter(|entry| entry.config.event == event)
        {
            let blocking = entry.config.blocking;
            if let (true, Some(reason)) = (blocking, &payload.too_large) {
                return Err(Blocked {
                    event,
                    hook: entry.name(),
                    reason: reason.clone(),
                });
            }
            let Err((failure, tail)) = self
                .execute(&entry.config, payload.text.as_bytes(), cwd)
                .await
            else {
                continue;
            };
            let reason = if tail.is_empty() {
                failure.describe()
            } else {
                format!("{}: {tail}", failure.describe())
            };
            let reason = one_line(&reason);
            if blocking {
                return Err(Blocked {
                    event,
                    hook: entry.name(),
                    reason,
                });
            }
            warn!(
                "hook failed, continuing: event={event} hook={} reason={reason}",
                entry.name()
            );
        }
        Ok(())
    }

    /// Runs one hook to its end. On failure returns how it failed and the tail
    /// of its standard error.
    async fn execute(
        &self,
        hook: &HookConfig,
        payload: &[u8],
        cwd: &Path,
    ) -> Result<(), (Failure, String)> {
        let run = Run {
            command: &hook.command,
            payload,
            cwd,
            timeout: hook.timeout(),
            capture: Capture::StderrOnFailure,
        };
        let finished = self.run_process(&run).await;
        finished
            .failure
            .map_or(Ok(()), |failure| Err((failure, finished.output)))
    }

    /// Runs `command` in `cwd` as a hook is run (no shell, the runner's
    /// environment, its whole process group killed at the end of `timeout` or
    /// when the call is dropped) and keeps the last `keep_bytes` of what it
    /// wrote to standard output and error together, whether it passed or not.
    pub async fn run_command(
        &self,
        command: &[String],
        cwd: &Path,
        timeout: Duration,
        keep_bytes: usize,
    ) -> CommandRun {
        let run = Run {
            command,
            payload: &[],
            cwd,
            timeout,
            capture: Capture::BothAlways(keep_bytes),
        };
        let finished = self.run_process(&run).await;
        let end = match finished.failure {
            None => CommandEnd::Passed,
            Some(Failure::Exited(_)) => CommandEnd::Failed(finished.failure_text()),
            Some(Failure::TimedOut(limit)) => CommandEnd::TimedOut(limit),
            Some(_) => CommandEnd::NotRun(finished.failure_text()),
        };
        CommandRun {
            end,
            output: finished.output,
        }
    }

    async fn run_process(&self, run: &Run<'_>) -> Finished {
        let Some((program, arguments)) = run.command.split_first() else {
            return Finished::failed(Failure::EmptyCommand);
        };
        let program = match resolve(program, &self.env) {
            Ok(program) => program,
            Err(error) => return Finished::failed(Failure::Start(error)),
        };
        let both = matches!(run.capture, Capture::BothAlways(_));
        let spawned = Command::new(program)
            .args(arguments)
            .current_dir(run.cwd)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(if both { Stdio::piped() } else { Stdio::null() })
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => return Finished::failed(Failure::Start(error)),
        };
        // Declared after `child`, so a dropped future kills the group before
        // the child's own kill-on-drop reaps the leader.
        let mut group = Group::new(
            child
                .id()
                .and_then(|id| Pid::from_raw(i32::try_from(id).ok()?)),
        );
        let said = Arc::new(Mutex::new(Said::with_cap(run.capture.keep_bytes())));
        let mut readers = Vec::new();
        if let Some(pipe) = child.stderr.take() {
            readers.push(tokio::spawn(read_said(pipe, Arc::clone(&said))));
        }
        if let Some(pipe) = child.stdout.take() {
            readers.push(tokio::spawn(read_said(pipe, Arc::clone(&said))));
        }
        let stdin = child.stdin.take();
        // The input is written while the hook is waited for, never before: a
        // hook that does not read it, or leaves a process holding the pipe,
        // would otherwise block the write past the hook's own end.
        let finished = tokio::time::timeout(run.timeout, async {
            tokio::select! {
                status = child.wait() => status,
                () = feed(stdin, run.payload) => child.wait().await,
            }
        })
        .await;
        let (failure, reaped) = match finished {
            Ok(Ok(status)) if status.success() => (None, true),
            Ok(Ok(status)) => (Some(Failure::Exited(status)), true),
            Ok(Err(error)) => (Some(Failure::Wait(error)), false),
            Err(_) => (Some(Failure::TimedOut(run.timeout)), false),
        };
        if !reaped {
            group.kill();
            let _ = child.wait().await;
        }
        // The leader is reaped, so its group id may belong to someone else now.
        group.disarm();
        let output = if both || failure.is_some() {
            settle(readers, &said).await
        } else {
            readers.iter().for_each(JoinHandle::abort);
            String::new()
        };
        Finished { failure, output }
    }
}

/// What to run for [`HookRunner::run_process`].
struct Run<'a> {
    command: &'a [String],
    payload: &'a [u8],
    cwd: &'a Path,
    timeout: Duration,
    capture: Capture,
}

/// What of a process's output is kept.
#[derive(Clone, Copy)]
enum Capture {
    /// The end of standard error, and only when the process failed.
    StderrOnFailure,
    /// The end of standard output and error together, however it ended.
    BothAlways(usize),
}

impl Capture {
    fn keep_bytes(self) -> usize {
        match self {
            Self::StderrOnFailure => MAX_REASON_BYTES,
            Self::BothAlways(bytes) => bytes,
        }
    }
}

/// How a process ended and what it said.
struct Finished {
    failure: Option<Failure>,
    output: String,
}

impl Finished {
    fn failed(failure: Failure) -> Self {
        Self {
            failure: Some(failure),
            output: String::new(),
        }
    }

    fn failure_text(&self) -> String {
        self.failure
            .as_ref()
            .map(Failure::describe)
            .unwrap_or_default()
    }
}

/// How a command run by [`HookRunner::run_command`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandEnd {
    /// It exited with status 0.
    Passed,
    /// It exited with another status or was ended by a signal; says which.
    Failed(String),
    /// It was still running after the limit and was killed.
    TimedOut(Duration),
    /// It never ran; says why.
    NotRun(String),
}

/// A command run to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRun {
    pub end: CommandEnd,
    /// The end of what it wrote to standard output and error together.
    pub output: String,
}

/// The file to run for `program`. An absolute path is run as it is. A bare name
/// is looked up only in the absolute entries of the hook's `PATH`: an empty or
/// relative entry would find a file in the session directory, which the model
/// can write. Anything else, such as a relative path, is not run.
fn resolve(program: &str, env: &[(OsString, OsString)]) -> io::Result<PathBuf> {
    if program.contains(std::path::is_separator) {
        let path = PathBuf::from(program);
        if path.is_absolute() {
            return Ok(path);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("`{program}` is a relative path"),
        ));
    }
    let path = env.iter().find(|(name, _)| name.as_os_str() == PATH_VAR);
    let found = (path.into_iter())
        .flat_map(|(_, value)| std::env::split_paths(value))
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate));
    found.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("`{program}` is not in an absolute entry of PATH"),
        )
    })
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & EXECUTE_BITS != 0)
}

/// Writes `payload` to a hook and closes its input. A hook that ignores its
/// input closes the pipe early; that is fine.
async fn feed(stdin: Option<ChildStdin>, payload: &[u8]) {
    if let Some(mut stdin) = stdin {
        let _ = stdin.write_all(payload).await;
    }
}

/// What a process said, waiting at most [`READER_GRACE`] in all for the ends of
/// its pipes. A reader still waiting then is aborted, not left running, and what
/// it had read by then still counts.
async fn settle(readers: Vec<JoinHandle<()>>, said: &Mutex<Said>) -> String {
    let deadline = tokio::time::Instant::now() + READER_GRACE;
    for mut reader in readers {
        if tokio::time::timeout_at(deadline, &mut reader)
            .await
            .is_err()
        {
            // A process the hook left behind still holds the pipe.
            reader.abort();
        }
    }
    lock(said).tail()
}

fn lock(said: &Mutex<Said>) -> MutexGuard<'_, Said> {
    // The reader holds the lock only to copy bytes, so a poisoned one is still whole.
    said.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The process group of a started hook. It is killed when the hook's call is
/// dropped or runs out of time, while the leader is not yet reaped; once the
/// leader is reaped the group id may be reused, so nothing is signalled.
struct Group {
    pid: Option<Pid>,
    live: bool,
}

impl Group {
    fn new(pid: Option<Pid>) -> Self {
        Self { pid, live: true }
    }

    fn kill(&self) {
        if let (true, Some(group)) = (self.live, self.pid) {
            // An empty group is fine.
            let _ = kill_process_group(group, Signal::KILL);
        }
    }

    fn disarm(&mut self) {
        self.live = false;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The end of what a process wrote, shared with the tasks that read its pipes
/// so that a read cut short loses nothing already read. Older bytes are dropped
/// as newer ones arrive, down to `cap`, so a chatty process costs no memory.
struct Said {
    kept: Vec<u8>,
    /// Whether older bytes were dropped, so the start may split a character.
    dropped: bool,
    cap: usize,
}

impl Said {
    fn with_cap(cap: usize) -> Self {
        Self {
            kept: Vec::new(),
            dropped: false,
            cap,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.kept.extend_from_slice(bytes);
        if self.kept.len() > self.cap * 2 {
            self.kept.drain(..self.kept.len() - self.cap);
            self.dropped = true;
        }
    }

    /// The last `cap` bytes as trimmed text, starting on a character boundary.
    fn tail(&self) -> String {
        let start = self.kept.len().saturating_sub(self.cap);
        let mut end = &self.kept[start..];
        if self.dropped || start > 0 {
            end = after_partial_character(end);
        }
        String::from_utf8_lossy(end).trim().to_owned()
    }
}

/// Reads `pipe` to its end into `said`.
async fn read_said(mut pipe: impl AsyncRead + Unpin, said: Arc<Mutex<Said>>) {
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => lock(&said).push(&chunk[..read]),
        }
    }
}

/// `bytes` without the leftover of a character a cut split at its start, which
/// would otherwise decode as a replacement character.
fn after_partial_character(bytes: &[u8]) -> &[u8] {
    let skip = bytes
        .iter()
        .position(|byte| byte & CONTINUATION_MASK != CONTINUATION_TAG)
        .unwrap_or(bytes.len());
    &bytes[skip..]
}

fn is_format(c: char) -> bool {
    FORMAT_RANGES
        .iter()
        .any(|(from, to)| (*from..=*to).contains(&c))
}

/// The first `cap` bytes of `text`, cut on a character boundary.
fn head(text: &str, cap: usize) -> &str {
    let mut end = text.len().min(cap);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `text` with its lines kept and no control or format characters except
/// line breaks and tabs, so a check cannot move the cursor of a terminal or hide
/// text through what it says.
pub(crate) fn plain_lines(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ESCAPE && chars.peek() == Some(&CSI_START) {
            // A colour or cursor sequence: skip to its final byte.
            chars.next();
            for next in chars.by_ref() {
                if CSI_FINAL.contains(&next) {
                    break;
                }
            }
        } else if matches!(c, '\n' | '\t')
            || !(c.is_control() || is_format(c) || LINE_SEPARATORS.contains(&c))
        {
            plain.push(c);
        }
    }
    plain
}

/// `text` on one line with no control or format characters, so that a hook
/// cannot forge a log line, move the cursor of a terminal or hide and reorder
/// text through what it says.
fn one_line(text: &str) -> String {
    let plain: String = (text.chars())
        .filter(|c| c.is_whitespace() || !(c.is_control() || is_format(*c)))
        .collect();
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}
