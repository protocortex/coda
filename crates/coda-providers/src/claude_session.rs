// SPDX-License-Identifier: AGPL-3.0-only

//! A supervised Claude Code child process speaking stream-json.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use coda_protocol::events::Event;
use coda_protocol::types::{PermissionDecision, RequestId, SessionId, TurnId};
use rustix::process::{Pid, Signal, kill_process_group};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::watch;
use tokio::time::timeout;

use crate::claude_code::{
    Diagnostic, Message, PermissionAsk, StreamParser, control_response, map_tool,
};
use crate::gate::PermissionGate;

/// Only flags proven against a live binary; `--permission-mode` is left out on purpose.
const CLAUDE_FLAGS: &[&str] = &[
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--permission-prompt-tool",
    "stdio",
];
const RESUME_FLAG: &str = "--resume";
/// Parent variables a delegated agent may inherit. Credentials such as
/// `ANTHROPIC_API_KEY` and `OPENAI_API_KEY`, and `CLAUDECODE`, are absent by design.
pub const ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "TMPDIR",
    "TZ",
];
pub(crate) const READ_CHUNK_BYTES: usize = 16 * 1024;
/// How long a child whose output ended gets to exit on its own, so its exit code survives.
pub(crate) const EXIT_GRACE: Duration = Duration::from_millis(500);
/// How long to wait for a killed child to be reaped before giving up on its status.
pub(crate) const REAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Why the adapter could not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// The Claude Code executable could not be started.
    #[error("could not start {}: {source}", .path.display())]
    Spawn {
        /// The binary that was tried.
        path: PathBuf,
        /// The operating system error.
        source: std::io::Error,
    },
    /// The child started without a piped standard stream.
    #[error("the Claude Code child has no {0} pipe")]
    MissingPipe(&'static str),
    /// Talking to the child failed.
    #[error("I/O with the Claude Code child failed: {0}")]
    Io(#[from] std::io::Error),
    /// A permission answer named a request that is not waiting.
    #[error("no permission request {request_id} is waiting for an answer")]
    UnknownRequest {
        /// The id that was answered.
        request_id: String,
    },
    /// The session was shut down or its process ended.
    #[error("the Claude Code session is closed")]
    Closed,
}

/// How one turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Claude Code reported the end of the turn.
    Completed {
        /// Whether the turn ended in an error.
        is_error: bool,
    },
    /// The process ended before the turn did.
    Crashed {
        /// Exit code, when the process had one.
        exit_code: Option<i32>,
    },
    /// The turn was cancelled and the process stopped.
    Cancelled,
}

/// What it takes to start a session.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    /// The Claude Code executable.
    pub binary: PathBuf,
    /// Working directory of the child and base for relative tool paths.
    pub cwd: PathBuf,
    /// The coda session the events belong to.
    pub session_id: SessionId,
    /// Claude Code session id to continue with `--resume`.
    pub resume: Option<String>,
    /// The parent environment; only allowlisted names reach the child.
    pub parent_env: Vec<(OsString, OsString)>,
}

/// Cancels a session's running or next turn; safe to call any number of times.
#[derive(Debug, Clone)]
pub struct CancelHandle(Arc<watch::Sender<bool>>);

impl CancelHandle {
    pub(crate) fn new(flag: Arc<watch::Sender<bool>>) -> Self {
        Self(flag)
    }

    /// Requests cancellation.
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
}

/// One Claude Code process and the turns run on it.
#[derive(Debug)]
pub struct ClaudeSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    parser: StreamParser,
    /// Messages parsed while a write was in flight, handled before the next read.
    backlog: VecDeque<Message>,
    /// Inputs of `can_use_tool` requests that still await an answer.
    pending: HashMap<String, Value>,
    cancel: Arc<watch::Sender<bool>>,
    session_id: SessionId,
    cwd: PathBuf,
    backend_session_id: Option<String>,
    closed: bool,
}

impl ClaudeSession {
    /// Starts the child in its own process group with the allowlisted environment.
    ///
    /// Must be called inside a tokio runtime.
    ///
    /// # Errors
    /// Returns [`AdapterError::Spawn`] when the binary cannot be started.
    pub fn spawn(spec: SessionSpec) -> Result<Self, AdapterError> {
        let mut command = Command::new(&spec.binary);
        command.args(CLAUDE_FLAGS);
        if let Some(resume) = &spec.resume {
            command.args([RESUME_FLAG, resume]);
        }
        command
            .env_clear()
            .envs(allowlisted(&spec.parent_env))
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command.spawn().map_err(|source| AdapterError::Spawn {
            path: spec.binary.clone(),
            source,
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or(AdapterError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(AdapterError::MissingPipe("stdout"))?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout,
            parser: StreamParser::new(),
            backlog: VecDeque::new(),
            pending: HashMap::new(),
            cancel: Arc::new(watch::channel(false).0),
            session_id: spec.session_id,
            cwd: spec.cwd,
            backend_session_id: None,
            closed: false,
        })
    }

    /// A handle that cancels this session from another task.
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle(Arc::clone(&self.cancel))
    }

    /// The child's process id, until it has been reaped.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Claude Code's own session id, once it has reported one.
    #[must_use]
    pub fn session_id_from_backend(&self) -> Option<&str> {
        self.backend_session_id.as_deref()
    }

    /// What the parser skipped so far; never carries message content.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        self.parser.diagnostics()
    }

    /// Sends one user message and drives the stream until the turn ends.
    ///
    /// Permission requests go through `gate`; every event is passed to `emit`.
    ///
    /// # Errors
    /// Returns [`AdapterError::Closed`] on a shut down session and
    /// [`AdapterError::Io`] when the child cannot be read or written.
    pub async fn run_turn<G: PermissionGate>(
        &mut self,
        turn_id: &TurnId,
        text: &str,
        gate: &G,
        emit: &mut impl FnMut(Event),
    ) -> Result<TurnOutcome, AdapterError> {
        if self.closed {
            return Err(AdapterError::Closed);
        }
        if *self.cancel.borrow() {
            return self.interrupted(turn_id, emit).await;
        }
        let user = json!({"type": "user", "message": {"role": "user", "content": text}});
        if !self.write_line(&user.to_string()).await? {
            return self.interrupted(turn_id, emit).await;
        }
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        loop {
            while let Some(message) = self.backlog.pop_front() {
                if let Some(outcome) = self.handle(message, turn_id, gate, emit).await? {
                    return Ok(outcome);
                }
            }
            let read = tokio::select! {
                read = self.stdout.read(&mut buffer) => read?,
                () = cancelled(self.cancel.subscribe()) => {
                    return self.interrupted(turn_id, emit).await;
                }
            };
            if read == 0 {
                return self.crashed(emit).await;
            }
            self.backlog.extend(self.parser.feed(&buffer[..read]));
        }
    }

    /// Answers a waiting `can_use_tool` request.
    ///
    /// # Errors
    /// Returns [`AdapterError::UnknownRequest`], writing nothing, when no such
    /// request is waiting, and [`AdapterError::Io`] when the write fails.
    pub async fn answer_permission(
        &mut self,
        request_id: &str,
        decision: PermissionDecision,
    ) -> Result<(), AdapterError> {
        let input =
            self.pending
                .remove(request_id)
                .ok_or_else(|| AdapterError::UnknownRequest {
                    request_id: request_id.to_owned(),
                })?;
        self.write_line(&control_response(request_id, decision, &input))
            .await?;
        Ok(())
    }

    /// Stops the whole process group and reaps the child; safe to repeat.
    ///
    /// # Errors
    /// Returns [`AdapterError::Io`] when the child cannot be reaped.
    pub async fn shutdown(&mut self) -> Result<(), AdapterError> {
        self.closed = true;
        self.stdin = None;
        self.kill_and_reap().await?;
        Ok(())
    }

    /// Kills the group, then waits a bounded time; `None` means the child was not reaped yet.
    async fn kill_and_reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
        if let Some(pid) = self.child.id() {
            kill_group(pid);
        }
        let _ = self.child.start_kill();
        match timeout(REAP_TIMEOUT, self.child.wait()).await {
            Ok(status) => status.map(Some),
            Err(_) => Ok(None),
        }
    }

    async fn handle<G: PermissionGate>(
        &mut self,
        message: Message,
        turn_id: &TurnId,
        gate: &G,
        emit: &mut impl FnMut(Event),
    ) -> Result<Option<TurnOutcome>, AdapterError> {
        match message {
            Message::Init { session_id } => self.backend_session_id = Some(session_id),
            Message::TextDelta(text) => emit(Event::StreamDelta {
                session_id: self.session_id.clone(),
                turn_id: turn_id.clone(),
                text,
            }),
            Message::Assistant { .. } => {}
            Message::Result { is_error } => {
                emit(Event::Result {
                    session_id: self.session_id.clone(),
                    turn_id: turn_id.clone(),
                    is_error,
                });
                return Ok(Some(TurnOutcome::Completed { is_error }));
            }
            Message::PermissionRequest(ask) => return self.ask(ask, turn_id, gate, emit).await,
            Message::InvalidPermissionRequest { request_id } => {
                let line = control_response(&request_id, PermissionDecision::Deny, &Value::Null);
                if !self.write_line(&line).await? {
                    return self.interrupted(turn_id, emit).await.map(Some);
                }
            }
        }
        Ok(None)
    }

    async fn ask<G: PermissionGate>(
        &mut self,
        ask: PermissionAsk,
        turn_id: &TurnId,
        gate: &G,
        emit: &mut impl FnMut(Event),
    ) -> Result<Option<TurnOutcome>, AdapterError> {
        let request = map_tool(&ask.tool_name, &ask.input, &self.cwd);
        self.pending.insert(ask.request_id.clone(), ask.input);
        let decision = tokio::select! {
            decision = gate.request(&request) => decision,
            () = cancelled(self.cancel.subscribe()) => {
                return self.interrupted(turn_id, emit).await.map(Some);
            }
        };
        let allowed = matches!(
            decision,
            PermissionDecision::AllowOnce | PermissionDecision::AllowAlways
        );
        let (session_id, request_id, tool) = (
            self.session_id.clone(),
            RequestId::new(ask.request_id.as_str()),
            request.tool.name().to_owned(),
        );
        emit(if allowed {
            Event::Approve {
                session_id,
                request_id,
                tool,
            }
        } else {
            Event::Deny {
                session_id,
                request_id,
                tool,
            }
        });
        let input = self.pending.remove(&ask.request_id).unwrap_or(Value::Null);
        let line = control_response(&ask.request_id, decision, &input);
        if !self.write_line(&line).await? {
            return self.interrupted(turn_id, emit).await.map(Some);
        }
        Ok(None)
    }

    async fn interrupted(
        &mut self,
        turn_id: &TurnId,
        emit: &mut impl FnMut(Event),
    ) -> Result<TurnOutcome, AdapterError> {
        emit(Event::TurnInterrupted {
            session_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
        });
        self.shutdown().await?;
        Ok(TurnOutcome::Cancelled)
    }

    async fn crashed(&mut self, emit: &mut impl FnMut(Event)) -> Result<TurnOutcome, AdapterError> {
        self.closed = true;
        self.stdin = None;
        let pid = self.child.id();
        // A child that closed stdout may still be running, and so may its descendants.
        let status = match timeout(EXIT_GRACE, self.child.wait()).await {
            Ok(status) => {
                if let Some(pid) = pid {
                    kill_group(pid);
                }
                Some(status?)
            }
            Err(_) => self.kill_and_reap().await?,
        };
        let exit_code = status.and_then(|status| status.code());
        emit(Event::BackendCrashed {
            session_id: self.session_id.clone(),
            exit_code,
            will_restart: false,
        });
        Ok(TurnOutcome::Crashed { exit_code })
    }

    /// Writes one line while draining stdout, so a child blocked on its own output
    /// cannot stall the write. Returns `false` when cancellation stopped the write.
    async fn write_line(&mut self, line: &str) -> Result<bool, AdapterError> {
        let stdin = self.stdin.as_mut().ok_or(AdapterError::Closed)?;
        let mut data = Vec::with_capacity(line.len() + 1);
        data.extend_from_slice(line.as_bytes());
        data.push(b'\n');
        let write = async {
            stdin.write_all(&data).await?;
            stdin.flush().await
        };
        tokio::pin!(write);
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        let mut output_open = true;
        loop {
            tokio::select! {
                result = &mut write => {
                    result?;
                    return Ok(true);
                }
                () = cancelled(self.cancel.subscribe()) => return Ok(false),
                read = self.stdout.read(&mut buffer), if output_open => {
                    let read = read?;
                    if read == 0 {
                        output_open = false;
                    } else {
                        self.backlog.extend(self.parser.feed(&buffer[..read]));
                    }
                }
            }
        }
    }
}

impl Drop for ClaudeSession {
    fn drop(&mut self) {
        // `kill_on_drop` only reaches the leader; descendants need the group signal.
        if let Some(pid) = self.child.id() {
            kill_group(pid);
        }
    }
}

/// The parent variables a child may inherit.
pub(crate) fn allowlisted(
    parent_env: &[(OsString, OsString)],
) -> impl Iterator<Item = (&OsString, &OsString)> {
    parent_env
        .iter()
        .filter(|(name, _)| ENV_ALLOWLIST.iter().any(|allowed| name == allowed))
        .map(|(name, value)| (name, value))
}

/// Sends SIGKILL to the process group led by `pid`; the group may already be gone.
pub(crate) fn kill_group(pid: u32) {
    let group = i32::try_from(pid).ok().and_then(Pid::from_raw);
    if let Some(group) = group {
        let _ = kill_process_group(group, Signal::KILL);
    }
}

pub(crate) async fn cancelled(mut flag: watch::Receiver<bool>) {
    // An error means every sender is gone; the session owns one, so treat it as not cancelled.
    if flag.wait_for(|cancelled| *cancelled).await.is_err() {
        std::future::pending::<()>().await;
    }
}
