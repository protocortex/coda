// SPDX-License-Identifier: AGPL-3.0-only

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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdin, Command};
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
/// The most bytes of a hook's standard error kept as the reason it failed.
pub const MAX_REASON_BYTES: usize = 1024;
/// The reason a blocking hook stops a call it could not be given whole: `what`
/// names the part of the call that went over its cap.
fn too_large(what: &str, cap: usize) -> String {
    format!("{what} too large for the hook to check (over {cap} bytes)")
}
/// The most of a hook's standard error held at once. Older bytes are dropped as
/// newer ones arrive, down to [`MAX_REASON_BYTES`], so a chatty hook costs no memory.
const TAIL_BUFFER_BYTES: usize = MAX_REASON_BYTES * 2;
/// How long to wait for the end of a failed hook's standard error once the hook
/// is gone, in case a process it left behind still holds the pipe.
const READER_GRACE: Duration = Duration::from_millis(500);
const READ_CHUNK_BYTES: usize = 4 * 1024;
/// Names a hook that has no program, in a block message or a log line.
const NO_PROGRAM: &str = "(no program)";
/// The variable a bare program name is looked up in.
const PATH_VAR: &str = "PATH";
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

    /// Whether any hook is set for `event`, so a payload is built only for one.
    fn has(&self, event: HookEvent) -> bool {
        self.hooks.iter().any(|entry| entry.config.event == event)
    }

    /// Runs the `session_start` hooks. `resumed` tells them the conversation
    /// had earlier messages.
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
        let Some((program, arguments)) = hook.command.split_first() else {
            return Err((Failure::EmptyCommand, String::new()));
        };
        let program =
            resolve(program, &self.env).map_err(|error| (Failure::Start(error), String::new()))?;
        let spawned = Command::new(program)
            .args(arguments)
            .current_dir(cwd)
            .env_clear()
            .envs(self.env.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn();
        let mut child = spawned.map_err(|error| (Failure::Start(error), String::new()))?;
        // Declared after `child`, so a dropped future kills the group before
        // the child's own kill-on-drop reaps the leader.
        let mut group = Group::new(
            child
                .id()
                .and_then(|id| Pid::from_raw(i32::try_from(id).ok()?)),
        );
        let said = Arc::new(Mutex::new(Said::default()));
        let stderr =
            (child.stderr.take()).map(|pipe| tokio::spawn(read_said(pipe, Arc::clone(&said))));
        let stdin = child.stdin.take();
        // The input is written while the hook is waited for, never before: a
        // hook that does not read it, or leaves a process holding the pipe,
        // would otherwise block the write past the hook's own end.
        let finished = tokio::time::timeout(hook.timeout(), async {
            tokio::select! {
                status = child.wait() => status,
                () = feed(stdin, payload) => child.wait().await,
            }
        })
        .await;
        let (failure, reaped) = match finished {
            Ok(Ok(status)) if status.success() => (None, true),
            Ok(Ok(status)) => (Some(Failure::Exited(status)), true),
            Ok(Err(error)) => (Some(Failure::Wait(error)), false),
            Err(_) => (Some(Failure::TimedOut(hook.timeout())), false),
        };
        if !reaped {
            group.kill();
            let _ = child.wait().await;
        }
        // The leader is reaped, so its group id may belong to someone else now.
        group.disarm();
        let tail = match (stderr, &failure) {
            (Some(reader), Some(_)) => settle(reader, &said).await,
            (Some(reader), None) => {
                reader.abort();
                String::new()
            }
            (None, _) => String::new(),
        };
        failure.map_or(Ok(()), |failure| Err((failure, tail)))
    }
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

/// What a failed hook said on standard error, waiting at most [`READER_GRACE`]
/// for the end of it. A reader still waiting then is aborted, not left running,
/// and what it had read by then still counts.
async fn settle(mut reader: JoinHandle<()>, said: &Mutex<Said>) -> String {
    if tokio::time::timeout(READER_GRACE, &mut reader)
        .await
        .is_err()
    {
        // A process the hook left behind still holds the pipe.
        reader.abort();
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

/// The end of what a hook wrote to standard error, shared with the task that
/// reads the pipe so that a read cut short loses nothing already read.
#[derive(Default)]
struct Said {
    kept: Vec<u8>,
    /// Whether older bytes were dropped, so the start may split a character.
    dropped: bool,
}

impl Said {
    fn push(&mut self, bytes: &[u8]) {
        self.kept.extend_from_slice(bytes);
        if self.kept.len() > TAIL_BUFFER_BYTES {
            self.kept.drain(..self.kept.len() - MAX_REASON_BYTES);
            self.dropped = true;
        }
    }

    /// The last [`MAX_REASON_BYTES`] as trimmed text, starting on a character boundary.
    fn tail(&self) -> String {
        let start = self.kept.len().saturating_sub(MAX_REASON_BYTES);
        let mut end = &self.kept[start..];
        if self.dropped || start > 0 {
            end = after_partial_character(end);
        }
        String::from_utf8_lossy(end).trim().to_owned()
    }
}

/// Reads `pipe` to its end into `said`.
async fn read_said(mut pipe: ChildStderr, said: Arc<Mutex<Said>>) {
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

/// `text` on one line with no control or format characters, so that a hook
/// cannot forge a log line, move the cursor of a terminal or hide and reorder
/// text through what it says.
fn one_line(text: &str) -> String {
    let plain: String = (text.chars())
        .filter(|c| c.is_whitespace() || !(c.is_control() || is_format(*c)))
        .collect();
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}
