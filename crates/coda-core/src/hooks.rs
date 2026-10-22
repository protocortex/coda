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
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use coda_config::hooks::{HookConfig, HookEvent};
use coda_protocol::types::SessionId;
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, Command};
use tokio::task::JoinHandle;
use tracing::warn;

/// The most bytes of a call's arguments a hook's payload carries.
pub const MAX_ARGUMENTS_BYTES: usize = 8 * 1024;
/// The most bytes of a call's result a hook's payload carries.
pub const MAX_RESULT_BYTES: usize = 4 * 1024;
/// The most bytes of a hook's standard error kept as the reason it failed.
pub const MAX_REASON_BYTES: usize = 1024;
/// The most of a hook's standard error held at once. Older bytes are dropped as
/// newer ones arrive, down to [`MAX_REASON_BYTES`], so a chatty hook costs no memory.
const TAIL_BUFFER_BYTES: usize = MAX_REASON_BYTES * 2;
/// How long to wait for the end of a failed hook's standard error once the hook
/// is gone, in case a process it left behind still holds the pipe.
const READER_GRACE: Duration = Duration::from_millis(500);
const READ_CHUNK_BYTES: usize = 4 * 1024;
/// Names a hook that has no program, in a block message or a log line.
const NO_PROGRAM: &str = "(no program)";
/// A UTF-8 continuation byte is `10xxxxxx`.
const CONTINUATION_MASK: u8 = 0b1100_0000;
const CONTINUATION_TAG: u8 = 0b1000_0000;

/// A blocking hook failed, so the action it guards does not happen.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("blocked by the {event} hook `{hook}`: {reason}")]
pub struct Blocked {
    pub event: HookEvent,
    /// The hook's program.
    pub hook: String,
    /// The end of the hook's standard error, or else how it ended.
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

/// The hooks of a session and the environment they run with.
#[derive(Debug, Clone, Default)]
pub struct HookRunner {
    hooks: Vec<HookConfig>,
    env: Vec<(OsString, OsString)>,
}

impl HookRunner {
    /// A runner for `hooks`. Their processes see `env` and nothing else.
    #[must_use]
    pub fn new(hooks: &[HookConfig], env: Vec<(OsString, OsString)>) -> Self {
        Self {
            hooks: hooks.to_vec(),
            env,
        }
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
        self.run(HookEvent::SessionStart, cwd, payload.to_string())
            .await
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
        if text.len() <= MAX_ARGUMENTS_BYTES {
            payload["arguments"] = arguments.clone();
        } else {
            // The head of the JSON text, which is no longer valid JSON.
            payload["arguments"] = json!(head(&text, MAX_ARGUMENTS_BYTES));
            payload["arguments_truncated"] = json!(true);
        }
        self.run(HookEvent::PreTool, cwd, payload.to_string()).await
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
        if result.len() > MAX_RESULT_BYTES {
            payload["result_truncated"] = json!(true);
        }
        self.run(HookEvent::PostTool, cwd, payload.to_string())
            .await
    }

    /// Whether any hook is set for `event`, so a payload is built only for one.
    fn has(&self, event: HookEvent) -> bool {
        self.hooks.iter().any(|hook| hook.event == event)
    }

    /// Runs the hooks of `event` in order. The first blocking hook that fails
    /// ends the run; one that is not blocking is logged.
    async fn run(&self, event: HookEvent, cwd: &Path, payload: String) -> Result<(), Blocked> {
        for hook in self.hooks.iter().filter(|hook| hook.event == event) {
            let Err((failure, tail)) = self.execute(hook, payload.as_bytes(), cwd).await else {
                continue;
            };
            let program = hook.command.first().map_or(NO_PROGRAM, String::as_str);
            if hook.blocking {
                let reason = if tail.is_empty() {
                    failure.describe()
                } else {
                    tail
                };
                return Err(Blocked {
                    event,
                    hook: program.to_owned(),
                    reason,
                });
            }
            let reason = if tail.is_empty() {
                failure.describe()
            } else {
                format!("{}: {tail}", failure.describe())
            };
            warn!(
                "hook failed, continuing: event={event} hook={program} reason={}",
                one_line(&reason)
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
        let stderr = child.stderr.take().map(|pipe| tokio::spawn(tail(pipe)));
        let stdin = child.stdin.take();
        let finished = tokio::time::timeout(hook.timeout(), async {
            if let Some(mut stdin) = stdin {
                // A hook that ignores its input closes the pipe early; that is fine.
                let _ = stdin.write_all(payload).await;
            }
            child.wait().await
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
            (Some(reader), Some(_)) => settle(reader).await,
            (Some(reader), None) => {
                reader.abort();
                String::new()
            }
            (None, _) => String::new(),
        };
        failure.map_or(Ok(()), |failure| Err((failure, tail)))
    }
}

/// The standard error tail of a failed hook, waiting at most [`READER_GRACE`]
/// for it. A reader still waiting then is aborted, not left running.
async fn settle(mut reader: JoinHandle<String>) -> String {
    if let Ok(joined) = tokio::time::timeout(READER_GRACE, &mut reader).await {
        return joined.unwrap_or_default();
    }
    // A process the hook left behind still holds the pipe.
    reader.abort();
    String::new()
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

/// Reads `pipe` to its end and returns the last [`MAX_REASON_BYTES`] of it as
/// trimmed text, starting on a character boundary.
async fn tail(mut pipe: ChildStderr) -> String {
    let mut kept: Vec<u8> = Vec::new();
    let mut dropped = false;
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                kept.extend_from_slice(&chunk[..read]);
                if kept.len() > TAIL_BUFFER_BYTES {
                    kept.drain(..kept.len() - MAX_REASON_BYTES);
                    dropped = true;
                }
            }
        }
    }
    let start = kept.len().saturating_sub(MAX_REASON_BYTES);
    let mut end = &kept[start..];
    if dropped || start > 0 {
        end = after_partial_character(end);
    }
    String::from_utf8_lossy(end).trim().to_owned()
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

/// The first `cap` bytes of `text`, cut on a character boundary.
fn head(text: &str, cap: usize) -> &str {
    let mut end = text.len().min(cap);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// `text` on one line, so a log line stays one line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
