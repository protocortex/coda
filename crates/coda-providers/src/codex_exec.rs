// SPDX-License-Identifier: AGPL-3.0-only

//! The Codex `exec --json` backend: one supervised process per turn.
//!
//! The event shapes parsed here follow the documented JSONL event names and
//! have only been exercised against the synthetic `fake-codex`. Verifying
//! them against a live `codex` is still open.

use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use coda_protocol::events::Event;
use coda_protocol::types::{SessionId, TokenUsage, TurnId};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::watch;
use tokio::time::timeout;

use crate::claude_code::Diagnostic;
use crate::claude_session::{
    CancelHandle, EXIT_GRACE, READ_CHUNK_BYTES, REAP_TIMEOUT, TurnOutcome, allowlisted, cancelled,
    kill_group,
};
use crate::jsonl::{DiagnosticLog, Framed, LineFramer, bounded};

const EXEC_ARGS: &[&str] = &["exec", "--json", "--sandbox"];
const RESUME_ARG: &str = "resume";
/// Tells `codex exec` to read the prompt from stdin, which keeps it out of the process list.
const STDIN_PROMPT_ARG: &str = "-";
const MAX_THREAD_ID_BYTES: usize = 128;

/// The future a [`SpawnHook`] returns; `Err` carries the reason it failed.
pub type SpawnHookFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// Called with the process id of each child right after it starts, so the caller
/// can record it; a failure stops the child and fails the turn.
pub type SpawnHook = Arc<dyn Fn(u32) -> SpawnHookFuture + Send + Sync>;

/// Gives the hook the `Debug` the session derives.
#[derive(Clone)]
struct HookSlot(SpawnHook);

impl std::fmt::Debug for HookSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpawnHook")
    }
}

/// A thread id is passed as a command-line argument, so it must never parse as a flag.
fn is_valid_thread_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_THREAD_ID_BYTES
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// What the Codex process may touch; anything wider than read-only needs explicit config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SandboxMode {
    /// No writes anywhere.
    #[default]
    ReadOnly,
    /// Writes inside the working directory.
    WorkspaceWrite,
    /// No sandbox.
    DangerFullAccess,
}

impl SandboxMode {
    pub(crate) fn as_arg(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

/// Why the adapter could not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum CodexError {
    /// The Codex executable could not be started.
    #[error("could not start {}: {source}", .path.display())]
    Spawn {
        /// The binary that was tried.
        path: PathBuf,
        /// The operating system error.
        source: std::io::Error,
    },
    /// The child started without a piped standard stream.
    #[error("the Codex child has no {0} pipe")]
    MissingPipe(&'static str),
    /// Talking to the child failed.
    #[error("I/O with the Codex child failed: {0}")]
    Io(#[from] std::io::Error),
    /// The session was shut down.
    #[error("the Codex session is closed")]
    Closed,
    /// The thread id to resume is not a plain identifier.
    #[error("the Codex thread id to resume is not a valid identifier")]
    InvalidThreadId,
    /// The caller could not record the started child.
    #[error("could not record the started Codex child: {0}")]
    SpawnHook(String),
}

/// What it takes to start a session.
#[derive(Debug, Clone)]
pub struct CodexSpec {
    /// The Codex executable.
    pub binary: PathBuf,
    /// Working directory of the child.
    pub cwd: PathBuf,
    /// The coda session the events belong to.
    pub session_id: SessionId,
    /// Codex thread id to continue with `exec resume`.
    pub resume: Option<String>,
    /// Sandbox the child runs under.
    pub sandbox: SandboxMode,
    /// The parent environment; only allowlisted names reach the child.
    pub parent_env: Vec<(OsString, OsString)>,
}

/// One message from the `exec --json` stream that coda acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexMessage {
    /// The thread started; carries the id used with `exec resume`.
    ThreadStarted {
        /// Codex's thread id.
        thread_id: String,
    },
    /// A complete assistant message; `exec` has no text deltas.
    AgentMessage(String),
    /// The turn finished with its token usage.
    TurnCompleted(TokenUsage),
    /// The turn failed.
    TurnFailed,
}

/// Turns raw output bytes into [`CodexMessage`]s, tolerating bad input.
#[derive(Debug, Default)]
pub struct CodexParser {
    framer: LineFramer,
    log: DiagnosticLog,
}

impl CodexParser {
    /// An empty parser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds bytes as read; a line split across calls is held until complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<CodexMessage> {
        let mut messages = Vec::new();
        for framed in self.framer.feed(bytes) {
            match framed {
                Framed::Line { number, bytes } => {
                    messages.extend(self.parse(&String::from_utf8_lossy(&bytes), number));
                }
                Framed::TooLong { number } => {
                    self.log.record(Diagnostic::LineTooLong { line: number });
                }
            }
        }
        messages
    }

    /// The first diagnostics recorded, in stream order; never carries message content.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        self.log.items()
    }

    fn parse(&mut self, text: &str, line: u64) -> Option<CodexMessage> {
        if text.trim().is_empty() {
            return None;
        }
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            self.log.record(Diagnostic::MalformedLine { line });
            return None;
        };
        match value["type"].as_str().unwrap_or_default() {
            "thread.started" => {
                let thread_id = value["thread_id"]
                    .as_str()
                    .filter(|id| is_valid_thread_id(id));
                if thread_id.is_none() {
                    self.log.record(Diagnostic::InvalidThreadId { line });
                }
                thread_id.map(|id| CodexMessage::ThreadStarted {
                    thread_id: id.to_owned(),
                })
            }
            "item.completed" if value["item"]["type"] == "agent_message" => value["item"]["text"]
                .as_str()
                .map(|text| CodexMessage::AgentMessage(text.to_owned())),
            "turn.completed" => Some(CodexMessage::TurnCompleted(usage_of(&value["usage"]))),
            "turn.failed" => Some(CodexMessage::TurnFailed),
            // Known events coda has no use for.
            "turn.started" | "item.started" | "item.updated" | "item.completed" | "error" => None,
            other => {
                self.log.record(Diagnostic::UnknownType {
                    line,
                    kind: bounded(other),
                });
                None
            }
        }
    }
}

fn usage_of(usage: &Value) -> TokenUsage {
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    TokenUsage {
        input_tokens: count("input_tokens"),
        output_tokens: count("output_tokens"),
        cache_read_tokens: count("cached_input_tokens"),
        cache_write_tokens: 0,
    }
}

/// A Codex session: a thread whose turns each run one `codex exec` process.
#[derive(Debug)]
pub struct CodexExecSession {
    spec: CodexSpec,
    child: Option<Child>,
    parser: CodexParser,
    cancel: Arc<watch::Sender<bool>>,
    backend_session_id: Option<String>,
    spawn_hook: Option<HookSlot>,
    closed: bool,
}

impl CodexExecSession {
    /// A session that starts its first process on the first turn.
    #[must_use]
    pub fn new(spec: CodexSpec) -> Self {
        Self {
            backend_session_id: spec.resume.clone(),
            spec,
            child: None,
            parser: CodexParser::new(),
            cancel: Arc::new(watch::channel(false).0),
            spawn_hook: None,
            closed: false,
        }
    }

    /// Calls `hook` with the pid of every child this session starts.
    #[must_use]
    pub fn with_spawn_hook(mut self, hook: SpawnHook) -> Self {
        self.spawn_hook = Some(HookSlot(hook));
        self
    }

    /// A handle that cancels this session from another task.
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::new(Arc::clone(&self.cancel))
    }

    /// The running child's process id; `None` between turns.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// Codex's own thread id, once known.
    #[must_use]
    pub fn session_id_from_backend(&self) -> Option<&str> {
        self.backend_session_id.as_deref()
    }

    /// What the parser skipped in the latest turn; never carries message content.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        self.parser.diagnostics()
    }

    /// Runs one prompt to the end of the turn, passing every event to `emit`.
    ///
    /// A process that dies mid-turn ends the turn as [`TurnOutcome::Crashed`];
    /// the next turn starts a fresh process that resumes the thread.
    ///
    /// # Errors
    /// Returns [`CodexError::Closed`] on a shut down session,
    /// [`CodexError::Spawn`] when the binary cannot start and
    /// [`CodexError::Io`] when the child cannot be read, [`CodexError::SpawnHook`] when its
    /// start cannot be recorded, and
    /// [`CodexError::InvalidThreadId`] when the thread to resume is not a plain identifier.
    pub async fn run_turn(
        &mut self,
        turn_id: &TurnId,
        text: &str,
        emit: &mut impl FnMut(Event),
    ) -> Result<TurnOutcome, CodexError> {
        if self.closed {
            return Err(CodexError::Closed);
        }
        if *self.cancel.borrow() {
            return self.interrupted(turn_id, emit).await;
        }
        self.parser = CodexParser::new();
        let spawned = self.spawn()?;
        self.announce_spawn().await?;
        self.supervise(turn_id, text, emit, spawned).await
    }

    async fn announce_spawn(&mut self) -> Result<(), CodexError> {
        let (Some(HookSlot(hook)), Some(pid)) = (self.spawn_hook.clone(), self.pid()) else {
            return Ok(());
        };
        if let Err(reason) = hook(pid).await {
            let _ = self.reap(Duration::ZERO).await;
            return Err(CodexError::SpawnHook(reason));
        }
        Ok(())
    }

    /// Runs the exchange; an I/O error also stops the group so no descendant leaks.
    async fn supervise<R: AsyncRead + Unpin>(
        &mut self,
        turn_id: &TurnId,
        text: &str,
        emit: &mut impl FnMut(Event),
        spawned: (ChildStdin, R),
    ) -> Result<TurnOutcome, CodexError> {
        let result = self.exchange(turn_id, text, emit, spawned).await;
        if result.is_err() {
            let _ = self.reap(Duration::ZERO).await;
        }
        result
    }

    async fn exchange<R: AsyncRead + Unpin>(
        &mut self,
        turn_id: &TurnId,
        text: &str,
        emit: &mut impl FnMut(Event),
        (stdin, mut stdout): (ChildStdin, R),
    ) -> Result<TurnOutcome, CodexError> {
        if !self.send_prompt(stdin, text).await? {
            return self.interrupted(turn_id, emit).await;
        }
        let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
        loop {
            let read = tokio::select! {
                read = stdout.read(&mut buffer) => read?,
                () = cancelled(self.cancel.subscribe()) => {
                    return self.interrupted(turn_id, emit).await;
                }
            };
            if read == 0 {
                return self.crashed(emit).await;
            }
            for message in self.parser.feed(&buffer[..read]) {
                if let Some(is_error) = self.handle(message, turn_id, emit) {
                    self.reap(EXIT_GRACE).await?;
                    return Ok(TurnOutcome::Completed { is_error });
                }
            }
        }
    }

    /// Stops the process group and refuses further turns; safe to repeat.
    ///
    /// # Errors
    /// Returns [`CodexError::Io`] when the child cannot be reaped.
    pub async fn shutdown(&mut self) -> Result<(), CodexError> {
        self.closed = true;
        self.reap(Duration::ZERO).await?;
        Ok(())
    }

    fn spawn(&mut self) -> Result<(ChildStdin, ChildStdout), CodexError> {
        let mut command = Command::new(&self.spec.binary);
        command.args(EXEC_ARGS).arg(self.spec.sandbox.as_arg());
        if let Some(thread_id) = &self.backend_session_id {
            if !is_valid_thread_id(thread_id) {
                return Err(CodexError::InvalidThreadId);
            }
            command.args([RESUME_ARG, thread_id]);
        }
        command
            .arg(STDIN_PROMPT_ARG)
            .env_clear()
            .envs(allowlisted(&self.spec.parent_env))
            .current_dir(&self.spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = command.spawn().map_err(|source| CodexError::Spawn {
            path: self.spec.binary.clone(),
            source,
        })?;
        let stdin = child.stdin.take().ok_or(CodexError::MissingPipe("stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(CodexError::MissingPipe("stdout"))?;
        self.child = Some(child);
        Ok((stdin, stdout))
    }

    /// Writes the prompt and closes stdin. Returns `false` when cancellation
    /// stopped the write; a child that already exited is left for the read loop.
    async fn send_prompt(&self, mut stdin: ChildStdin, text: &str) -> Result<bool, CodexError> {
        let write = async {
            match stdin.write_all(text.as_bytes()).await {
                Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => Err(error),
                _ => Ok(()),
            }
        };
        tokio::select! {
            result = write => {
                result?;
                Ok(true)
            }
            () = cancelled(self.cancel.subscribe()) => Ok(false),
        }
    }

    /// Emits the events of one message; returns the turn's error flag once it ended.
    fn handle(
        &mut self,
        message: CodexMessage,
        turn_id: &TurnId,
        emit: &mut impl FnMut(Event),
    ) -> Option<bool> {
        let session_id = self.spec.session_id.clone();
        let turn_id = turn_id.clone();
        match message {
            CodexMessage::ThreadStarted { thread_id } => self.backend_session_id = Some(thread_id),
            CodexMessage::AgentMessage(text) => emit(Event::StreamDelta {
                session_id,
                turn_id,
                text,
            }),
            CodexMessage::TurnCompleted(usage) => {
                emit(Event::Usage {
                    session_id: session_id.clone(),
                    turn_id: turn_id.clone(),
                    usage,
                });
                emit(Event::Result {
                    session_id,
                    turn_id,
                    is_error: false,
                });
                return Some(false);
            }
            CodexMessage::TurnFailed => {
                emit(Event::Result {
                    session_id,
                    turn_id,
                    is_error: true,
                });
                return Some(true);
            }
        }
        None
    }

    async fn interrupted(
        &mut self,
        turn_id: &TurnId,
        emit: &mut impl FnMut(Event),
    ) -> Result<TurnOutcome, CodexError> {
        emit(Event::TurnInterrupted {
            session_id: self.spec.session_id.clone(),
            turn_id: turn_id.clone(),
        });
        self.reap(Duration::ZERO).await?;
        Ok(TurnOutcome::Cancelled)
    }

    async fn crashed(&mut self, emit: &mut impl FnMut(Event)) -> Result<TurnOutcome, CodexError> {
        let exit_code = self
            .reap(EXIT_GRACE)
            .await?
            .and_then(|status| status.code());
        emit(Event::BackendCrashed {
            session_id: self.spec.session_id.clone(),
            exit_code,
            will_restart: false,
        });
        Ok(TurnOutcome::Crashed { exit_code })
    }

    /// Gives the child `grace` to exit on its own, then kills its whole group
    /// and reaps it. `None` means the child had no status to report.
    async fn reap(&mut self, grace: Duration) -> std::io::Result<Option<ExitStatus>> {
        let Some(mut child) = self.child.take() else {
            return Ok(None);
        };
        let pid = child.id();
        let mut status = match timeout(grace, child.wait()).await {
            Ok(status) => Some(status?),
            Err(_) => None,
        };
        // Descendants can outlive a leader that exited cleanly.
        if let Some(pid) = pid {
            kill_group(pid);
        }
        if status.is_none() {
            let _ = child.start_kill();
            status = timeout(REAP_TIMEOUT, child.wait()).await.ok().transpose()?;
        }
        Ok(status)
    }
}

impl Drop for CodexExecSession {
    fn drop(&mut self) {
        // `kill_on_drop` only reaches the leader; descendants need the group signal.
        if let Some(pid) = self.child.as_ref().and_then(Child::id) {
            kill_group(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::ReadBuf;

    use super::*;

    /// A stdout that fails once the fake child has started its grandchild.
    struct FailsOnceStarted(PathBuf);

    impl AsyncRead for FailsOnceStarted {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if std::fs::read_to_string(&self.0).is_ok_and(|pid| !pid.is_empty()) {
                return Poll::Ready(Err(std::io::Error::other("injected read failure")));
            }
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn an_io_error_mid_turn_stops_the_whole_process_group() {
        // Arrange
        let cwd = tempfile::tempdir().unwrap();
        let pid_path = cwd.path().join("grandchild.pid");
        let control = serde_json::json!({"pid_path": pid_path}).to_string();
        std::fs::write(cwd.path().join("fake-codex-control.json"), control).unwrap();
        let mut session = CodexExecSession::new(CodexSpec {
            binary: coda_testkit::paths::fake_codex_path().unwrap(),
            cwd: cwd.path().to_path_buf(),
            session_id: SessionId::new("s-1"),
            resume: None,
            sandbox: SandboxMode::default(),
            parent_env: Vec::new(),
        });
        let (stdin, _stdout) = session.spawn().unwrap();

        // Act
        let result = timeout(
            Duration::from_secs(30),
            session.supervise(
                &TurnId::new("t-1"),
                "grandchild",
                &mut |_| {},
                (stdin, FailsOnceStarted(pid_path.clone())),
            ),
        )
        .await
        .expect("turn did not finish in time");

        // Assert
        assert!(matches!(result, Err(CodexError::Io(_))));
        let grandchild = rustix::process::Pid::from_raw(
            std::fs::read_to_string(&pid_path)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
        )
        .unwrap();
        let gone = || rustix::process::test_kill_process(grandchild).is_err();
        for _ in 0..400 {
            if gone() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(gone(), "the grandchild outlived the failed turn");
    }

    // Synthetic fixtures: hand-written from the documented event names, not recorded.
    const THREAD: &str = r#"{"type":"thread.started","thread_id":"th-1"}"#;
    const MESSAGE: &str =
        r#"{"type":"item.completed","item":{"id":"i0","type":"agent_message","text":"hi"}}"#;
    const COMMAND: &str =
        r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"ls"}}"#;
    const DONE: &str = r#"{"type":"turn.completed","usage":{"input_tokens":5,"cached_input_tokens":2,"output_tokens":3}}"#;
    const FAILED: &str = r#"{"type":"turn.failed","error":{}}"#;

    fn feed_lines(parser: &mut CodexParser, lines: &[&str]) -> Vec<CodexMessage> {
        let mut input = lines.join("\n");
        input.push('\n');
        parser.feed(input.as_bytes())
    }

    #[test]
    fn only_plain_bounded_identifiers_are_valid_thread_ids() {
        // Arrange
        let valid = ["0199a213-81c0-7800-8aa1-bbab2a035a53", "thread_1"];
        let long = "a".repeat(MAX_THREAD_ID_BYTES + 1);
        let invalid = ["", "--dangerously-bypass", "-x", "a b", "a\nb", &long];

        // Act and Assert
        assert!(valid.iter().all(|id| is_valid_thread_id(id)));
        assert!(!invalid.iter().any(|id| is_valid_thread_id(id)));
    }

    #[test]
    fn known_events_are_parsed_and_other_items_ignored() {
        // Arrange
        let mut parser = CodexParser::new();

        // Act
        let messages = feed_lines(
            &mut parser,
            &[
                THREAD,
                r#"{"type":"turn.started"}"#,
                MESSAGE,
                COMMAND,
                DONE,
                FAILED,
            ],
        );

        // Assert
        assert_eq!(
            messages,
            vec![
                CodexMessage::ThreadStarted {
                    thread_id: "th-1".to_owned()
                },
                CodexMessage::AgentMessage("hi".to_owned()),
                CodexMessage::TurnCompleted(TokenUsage {
                    input_tokens: 5,
                    output_tokens: 3,
                    cache_read_tokens: 2,
                    cache_write_tokens: 0,
                }),
                CodexMessage::TurnFailed,
            ]
        );
        assert_eq!(parser.diagnostics(), []);
    }

    #[test]
    fn bad_lines_are_recorded_without_content_and_parsing_continues() {
        // Arrange
        let mut parser = CodexParser::new();
        let long_kind = format!(r#"{{"type":"{}"}}"#, "k".repeat(200));

        // Act
        let messages = feed_lines(
            &mut parser,
            &[
                "{not json",
                &long_kind,
                r#"{"type":"thread.started"}"#,
                DONE,
            ],
        );

        // Assert
        assert_eq!(messages.len(), 1);
        assert_eq!(
            parser.diagnostics(),
            [
                Diagnostic::MalformedLine { line: 1 },
                Diagnostic::UnknownType {
                    line: 2,
                    kind: "k".repeat(64)
                },
                Diagnostic::InvalidThreadId { line: 3 },
            ]
        );
    }
}
