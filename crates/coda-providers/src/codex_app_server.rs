// SPDX-License-Identifier: AGPL-3.0-only

//! The Codex `app-server` backend: one long-lived process speaking JSON over WebSocket.
//!
//! The server listens on a Unix socket in a directory only coda can enter. The
//! method names and message shapes handled here follow the documented protocol
//! and have only been exercised against the synthetic `fake-codex`; checking
//! them, and the value of an accepted decision, against a live `codex` is open.
//!
//! Known limit, pending live verification: cancelling a turn kills the server's
//! process group and closes the session. `turn/interrupt` is not used, so a
//! cancelled turn cannot be followed by another on the same session.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::time::Duration;

use coda_protocol::events::Event;
use coda_protocol::types::{PermissionDecision, RequestId, SessionId, TokenUsage, TurnId};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

use crate::claude_session::{
    CancelHandle, EXIT_GRACE, READ_CHUNK_BYTES, REAP_TIMEOUT, TurnOutcome, allowlisted, cancelled,
    kill_group,
};
use crate::codex_exec::{SandboxMode, SpawnHook};
use crate::gate::PermissionGate;
use crate::tool_request::{Tool, ToolRequest};
use crate::websocket::{
    FrameReader, Incoming, MAX_HEAD_BYTES, WsError, close_frame, parse_upgrade_response,
    pong_frame, text_frame, upgrade_request,
};

const LISTEN_ARGS: &[&str] = &["app-server", "--listen"];
const SOCKET_FILE: &str = "codex.sock";
/// Owner-only, so no other user can reach the socket.
const SOCKET_DIR_MODE: u32 = 0o700;
const SOCKET_DIR_PREFIX: &str = "cdx";
/// `sockaddr_un` holds 104 bytes on macOS and 108 on Linux, including the terminator.
const MAX_SOCKET_PATH_BYTES: usize = 100;
/// How long the server gets to create its socket, answer the upgrade and initialize.
const START_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_POLL: Duration = Duration::from_millis(20);
const APPROVAL_POLICY: &str = "on-request";
const CLIENT_NAME: &str = "coda";
const UPGRADED: u16 = 101;
const METHOD_NOT_FOUND: i64 = -32601;
/// Items remembered between `item/started` and the approval that refers to them.
const MAX_TRACKED_ITEMS: usize = 1024;
const ACCEPT: &str = "accept";
const DECLINE: &str = "decline";

/// Why the adapter could not do what was asked.
#[derive(Debug, thiserror::Error)]
pub enum AppServerError {
    /// The Codex executable could not be started.
    #[error("could not start {}: {source}", .path.display())]
    Spawn {
        /// The binary that was tried.
        path: PathBuf,
        /// The operating system error.
        source: std::io::Error,
    },
    /// The private socket directory could not be prepared.
    #[error("could not prepare the private socket directory: {0}")]
    SocketDir(std::io::Error),
    /// The socket path would not fit a Unix socket address.
    #[error("the socket path is over {MAX_SOCKET_PATH_BYTES} bytes; set a shorter TMPDIR")]
    SocketPathTooLong,
    /// The server did not become ready in time.
    #[error("the Codex app-server was not ready within {} seconds", START_TIMEOUT.as_secs())]
    StartTimeout,
    /// The server answered the upgrade with something other than 101.
    #[error("the Codex app-server refused the WebSocket upgrade with HTTP status {status}")]
    Upgrade {
        /// The HTTP status it answered with.
        status: u16,
    },
    /// The server process exited before it was ready.
    #[error("the Codex app-server exited before it was ready (exit code {exit_code:?})")]
    ExitedEarly {
        /// The exit code, when the process ended with one.
        exit_code: Option<i32>,
    },
    /// The server ended the connection before it was ready.
    #[error("the Codex app-server closed the connection before it was ready")]
    ClosedEarly,
    /// The server broke the WebSocket protocol.
    #[error("the Codex app-server broke the WebSocket protocol: {0}")]
    Protocol(#[from] WsError),
    /// The server answered a startup request with an error.
    #[error("the Codex app-server rejected `{method}`")]
    Rejected {
        /// The request method.
        method: &'static str,
    },
    /// The caller could not record the started server.
    #[error("could not record the started Codex app-server: {0}")]
    SpawnHook(String),
    /// Talking to the server failed.
    #[error("I/O with the Codex app-server failed: {0}")]
    Io(#[from] std::io::Error),
    /// The session was shut down.
    #[error("the Codex app-server session is closed")]
    Closed,
}

/// What it takes to start a session.
#[derive(Debug, Clone)]
pub struct AppServerSpec {
    /// The Codex executable.
    pub binary: PathBuf,
    /// Working directory of the server and of the thread.
    pub cwd: PathBuf,
    /// The coda session the events belong to.
    pub session_id: SessionId,
    /// Sandbox the thread runs under.
    pub sandbox: SandboxMode,
    /// Whether the thread is kept out of Codex's saved history.
    pub ephemeral: bool,
    /// Server versions known to behave; any other one raises a drift warning.
    pub verified_versions: Vec<String>,
    /// The parent environment; only allowlisted names reach the child.
    pub parent_env: Vec<(OsString, OsString)>,
}

/// An item whose details a later approval request refers to.
#[derive(Debug, Clone)]
struct Item {
    command: Option<String>,
    cwd: Option<PathBuf>,
    paths: Vec<PathBuf>,
}

impl Item {
    fn from_json(item: &Value) -> Self {
        Self {
            command: item["command"].as_str().map(str::to_owned),
            cwd: item["cwd"].as_str().map(PathBuf::from),
            paths: item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|change| change["path"].as_str().map(PathBuf::from))
                .collect(),
        }
    }
}

/// One message from the server, by shape.
#[derive(Debug)]
enum Message {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        failed: bool,
        result: Value,
    },
}

impl Message {
    fn parse(text: &str) -> Option<Self> {
        let mut value: Value = serde_json::from_str(text).ok()?;
        if !value.is_object() {
            debug!("ignored a Codex app-server message that is not a JSON object");
            return None;
        }
        let params = value["params"].take();
        let id = value["id"].take();
        match (value["method"].as_str(), id.is_null()) {
            (Some(method), true) => Some(Self::Notification {
                method: method.to_owned(),
                params,
            }),
            (Some(method), false) => Some(Self::Request {
                id,
                method: method.to_owned(),
                params,
            }),
            (None, false) => Some(Self::Response {
                id,
                failed: !value["error"].is_null(),
                result: value["result"].take(),
            }),
            (None, true) => None,
        }
    }
}

/// A Codex thread served by one `codex app-server` process.
#[derive(Debug)]
pub struct CodexAppServerSession {
    child: Option<Child>,
    stream: UnixStream,
    reader: FrameReader,
    socket_dir: tempfile::TempDir,
    cancel: Arc<watch::Sender<bool>>,
    session_id: SessionId,
    cwd: PathBuf,
    thread_id: String,
    server_version: String,
    drift: Option<String>,
    next_id: u64,
    items: HashMap<String, Item>,
    /// Messages that arrived while an approval was pending.
    queued: VecDeque<Message>,
    closed: bool,
}

impl CodexAppServerSession {
    /// Starts the server and opens the thread: socket, upgrade, `initialize`,
    /// `initialized` and `thread/start`. Must be called inside a tokio runtime.
    /// `on_spawn` sees the server's pid right after it starts, before the
    /// handshake, so a crash mid-start still leaves it recorded.
    ///
    /// # Errors
    /// Fails when the server cannot start, `on_spawn` fails, the server refuses the
    /// upgrade, breaks the protocol or is not ready in time; the child is stopped on
    /// every failure.
    pub async fn start(
        spec: AppServerSpec,
        on_spawn: Option<SpawnHook>,
    ) -> Result<Self, AppServerError> {
        let socket_dir = tempfile::Builder::new()
            .prefix(SOCKET_DIR_PREFIX)
            .permissions(std::fs::Permissions::from_mode(SOCKET_DIR_MODE))
            .tempdir()
            .map_err(AppServerError::SocketDir)?;
        let socket = socket_dir.path().join(SOCKET_FILE);
        if socket.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
            return Err(AppServerError::SocketPathTooLong);
        }
        let mut child = spawn_server(&spec, &socket)?;
        let mut guard = ChildGuard(child.id());
        if let (Some(hook), Some(pid)) = (on_spawn, child.id()) {
            hook(pid).await.map_err(AppServerError::SpawnHook)?;
        }
        let handshake = timeout(START_TIMEOUT, Self::handshake(&spec, &socket, &mut child)).await;
        let (stream, reader, version, thread_id) = match handshake {
            Ok(result) => result?,
            Err(_) => return Err(AppServerError::StartTimeout),
        };
        let drift = (!spec.verified_versions.contains(&version)).then(|| {
            let message = format!("Codex app-server version {version} is not in the verified list");
            warn!(version = %version, "{message}");
            message
        });
        guard.0 = None;
        Ok(Self {
            child: Some(child),
            stream,
            reader,
            socket_dir,
            cancel: Arc::new(watch::channel(false).0),
            session_id: spec.session_id,
            cwd: spec.cwd,
            thread_id,
            server_version: version,
            drift,
            next_id: 0,
            items: HashMap::new(),
            queued: VecDeque::new(),
            closed: false,
        })
    }

    async fn handshake(
        spec: &AppServerSpec,
        socket: &Path,
        child: &mut Child,
    ) -> Result<(UnixStream, FrameReader, String, String), AppServerError> {
        let mut stream = connect(socket, child).await?;
        let mut reader = FrameReader::default();
        upgrade(&mut stream, &mut reader).await?;
        let mut next_id = 0;
        let init =
            json!({"clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")}});
        let result = request(&mut stream, &mut reader, &mut next_id, "initialize", init).await?;
        let version = result["serverInfo"]["version"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let initialized = json!({"method": "initialized"}).to_string();
        stream.write_all(&text_frame(&initialized)).await?;
        let thread = json!({
            "cwd": spec.cwd,
            "sandbox": spec.sandbox.as_arg(),
            "approvalPolicy": APPROVAL_POLICY,
            "ephemeral": spec.ephemeral,
        });
        let result = request(
            &mut stream,
            &mut reader,
            &mut next_id,
            "thread/start",
            thread,
        )
        .await?;
        let thread_id = result["thread"]["id"]
            .as_str()
            .ok_or(AppServerError::Rejected {
                method: "thread/start",
            })?
            .to_owned();
        Ok((stream, reader, version, thread_id))
    }

    /// A handle that cancels this session from another task.
    #[must_use]
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::new(Arc::clone(&self.cancel))
    }

    /// The server's process id, until it has been reaped.
    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// Codex's id for the thread.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The version the server reported; empty when it reported none.
    #[must_use]
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// Set when the server version is not in the verified list.
    #[must_use]
    pub fn drift_warning(&self) -> Option<&str> {
        self.drift.as_deref()
    }

    /// The private socket the server listens on.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.socket_dir.path().join(SOCKET_FILE)
    }

    /// Runs one prompt to the end of the turn, passing every event to `emit`.
    ///
    /// Approval requests go through `gate`. A server that closes the connection
    /// or exits mid-turn ends the turn as [`TurnOutcome::Crashed`].
    ///
    /// # Errors
    /// Returns [`AppServerError::Closed`] on a shut down session and
    /// [`AppServerError::Io`] or [`AppServerError::Protocol`] when the connection
    /// fails; the server is stopped in those cases.
    pub async fn run_turn<G: PermissionGate>(
        &mut self,
        turn_id: &TurnId,
        text: &str,
        gate: &G,
        emit: &mut impl FnMut(Event),
    ) -> Result<TurnOutcome, AppServerError> {
        if self.closed {
            return Err(AppServerError::Closed);
        }
        if *self.cancel.borrow() {
            return self.interrupted(turn_id, emit).await;
        }
        let cancel = self.cancel.subscribe();
        let result = tokio::select! {
            result = self.drive(turn_id, text, gate, emit) => result,
            () = cancelled(cancel) => return self.interrupted(turn_id, emit).await,
        };
        match result {
            Ok(Some(outcome)) => Ok(outcome),
            Ok(None) => self.crashed(emit).await,
            Err(error) => {
                let _ = self.stop().await;
                Err(error)
            }
        }
    }

    /// Stops the whole process group and refuses further turns; safe to repeat.
    ///
    /// # Errors
    /// Returns [`AppServerError::Io`] when the child cannot be reaped.
    pub async fn shutdown(&mut self) -> Result<(), AppServerError> {
        self.stop().await?;
        Ok(())
    }

    /// Runs the exchange; `None` means the connection ended before the turn did.
    async fn drive<G: PermissionGate>(
        &mut self,
        turn_id: &TurnId,
        text: &str,
        gate: &G,
        emit: &mut impl FnMut(Event),
    ) -> Result<Option<TurnOutcome>, AppServerError> {
        let params = json!({"threadId": self.thread_id, "input": [{"type": "text", "text": text}]});
        let turn_request = self.send_request("turn/start", params).await?;
        let mut usage = None;
        while let Some(message) = self.next_message().await? {
            match message {
                Message::Notification { method, params } => {
                    if let Some(is_error) = self.notify(&method, &params, turn_id, &mut usage, emit)
                    {
                        return Ok(Some(TurnOutcome::Completed { is_error }));
                    }
                }
                Message::Request { id, method, params } => {
                    if !self.answer(&id, &method, &params, gate, emit).await? {
                        return Ok(None);
                    }
                }
                Message::Response {
                    id, failed: true, ..
                } if id == turn_request => {
                    emit(self.result(turn_id, true));
                    return Ok(Some(TurnOutcome::Completed { is_error: true }));
                }
                Message::Response { .. } => {}
            }
        }
        Ok(None)
    }

    /// Handles one notification; returns the turn's error flag once the turn ended.
    fn notify(
        &mut self,
        method: &str,
        params: &Value,
        turn_id: &TurnId,
        usage: &mut Option<TokenUsage>,
        emit: &mut impl FnMut(Event),
    ) -> Option<bool> {
        match method {
            "item/agentMessage/delta" => {
                if let Some(text) = params["delta"].as_str() {
                    emit(Event::StreamDelta {
                        session_id: self.session_id.clone(),
                        turn_id: turn_id.clone(),
                        text: text.to_owned(),
                    });
                }
            }
            "item/started" => {
                if let Some(id) = params["item"]["id"].as_str()
                    && self.items.len() < MAX_TRACKED_ITEMS
                {
                    self.items
                        .insert(id.to_owned(), Item::from_json(&params["item"]));
                }
            }
            "item/completed" => {
                if let Some(id) = params["item"]["id"].as_str() {
                    self.items.remove(id);
                }
            }
            "thread/tokenUsage/updated" => *usage = Some(usage_of(&params["tokenUsage"]["last"])),
            "turn/completed" => {
                let is_error = params["turn"]["status"] != "completed";
                if let Some(usage) = usage.take() {
                    emit(Event::Usage {
                        session_id: self.session_id.clone(),
                        turn_id: turn_id.clone(),
                        usage,
                    });
                }
                emit(self.result(turn_id, is_error));
                return Some(is_error);
            }
            other => debug!(method = other, "ignored Codex app-server notification"),
        }
        None
    }

    fn result(&self, turn_id: &TurnId, is_error: bool) -> Event {
        Event::Result {
            session_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
            is_error,
        }
    }

    /// Answers a server request: approvals go to the gate, anything else is refused.
    /// Returns `false` when the connection ended while the gate was deciding.
    async fn answer<G: PermissionGate>(
        &mut self,
        id: &Value,
        method: &str,
        params: &Value,
        gate: &G,
        emit: &mut impl FnMut(Event),
    ) -> Result<bool, AppServerError> {
        let Some(request) = self.tool_request(method, params) else {
            let error = json!({"code": METHOD_NOT_FOUND, "message": "method not supported"});
            self.send(&json!({"id": id, "error": error})).await?;
            return Ok(true);
        };
        // Never prompt for something the user cannot see.
        let decision = if shows_nothing(&request) {
            warn!(
                method,
                "declined an approval request that names no command or path"
            );
            PermissionDecision::Deny
        } else {
            match self.decide(gate, &request).await? {
                Some(decision) => decision,
                None => return Ok(false),
            }
        };
        let allowed = matches!(
            decision,
            PermissionDecision::AllowOnce | PermissionDecision::AllowAlways
        );
        let (session_id, tool) = (self.session_id.clone(), request.tool.name().to_owned());
        let request_id = RequestId::new(match id {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
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
        let verdict = if allowed { ACCEPT } else { DECLINE };
        self.send(&json!({"id": id, "result": {"decision": verdict}}))
            .await?;
        Ok(true)
    }

    /// Waits for the gate while still reading the socket, so pings are answered and a
    /// close is noticed. `None` means the connection ended before the gate decided.
    async fn decide<G: PermissionGate>(
        &mut self,
        gate: &G,
        request: &ToolRequest,
    ) -> Result<Option<PermissionDecision>, AppServerError> {
        let mut decision = pin!(gate.request(request));
        loop {
            // Only the read sits in the select: dropping it mid-write could corrupt a frame.
            let incoming = tokio::select! {
                decision = &mut decision => return Ok(Some(decision)),
                incoming = read_incoming(&mut self.stream, &mut self.reader) => incoming?,
            };
            match incoming {
                None => return Ok(None),
                Some(Incoming::Close) => {
                    self.reply_close().await;
                    return Ok(None);
                }
                Some(Incoming::Ping(payload)) => {
                    self.stream.write_all(&pong_frame(&payload)).await?;
                }
                Some(Incoming::Pong) => {}
                Some(Incoming::Text(text)) => {
                    if let Some(message) = Message::parse(&text) {
                        self.queued.push_back(message);
                    }
                }
            }
        }
    }

    /// Best effort: the server may already be gone.
    async fn reply_close(&mut self) {
        let _ = self.stream.write_all(&close_frame()).await;
    }

    fn tool_request(&self, method: &str, params: &Value) -> Option<ToolRequest> {
        let item = params["itemId"].as_str().and_then(|id| self.items.get(id));
        let cwd = params["cwd"]
            .as_str()
            .map(PathBuf::from)
            .or_else(|| item.and_then(|item| item.cwd.clone()))
            .unwrap_or_else(|| self.cwd.clone());
        match method {
            "item/commandExecution/requestApproval" => Some(ToolRequest {
                tool: Tool::Bash,
                target_paths: Vec::new(),
                command: params["command"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| item.and_then(|item| item.command.clone()))
                    .filter(|command| !command.trim().is_empty()),
                cwd,
            }),
            "item/fileChange/requestApproval" => Some(ToolRequest {
                tool: Tool::Edit,
                target_paths: item
                    .map(|item| item.paths.iter().map(|path| cwd.join(path)).collect())
                    .unwrap_or_default(),
                command: None,
                cwd,
            }),
            _ => None,
        }
    }

    async fn send_request(
        &mut self,
        method: &'static str,
        params: Value,
    ) -> Result<Value, AppServerError> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"id": id, "method": method, "params": params}))
            .await?;
        Ok(json!(id))
    }

    async fn send(&mut self, value: &Value) -> Result<(), AppServerError> {
        self.stream
            .write_all(&text_frame(&value.to_string()))
            .await?;
        Ok(())
    }

    /// The next JSON message, answering pings on the way; `None` once the connection ended.
    async fn next_message(&mut self) -> Result<Option<Message>, AppServerError> {
        if let Some(message) = self.queued.pop_front() {
            return Ok(Some(message));
        }
        loop {
            match read_incoming(&mut self.stream, &mut self.reader).await? {
                None => return Ok(None),
                Some(Incoming::Close) => {
                    self.reply_close().await;
                    return Ok(None);
                }
                Some(Incoming::Ping(payload)) => {
                    self.stream.write_all(&pong_frame(&payload)).await?;
                }
                Some(Incoming::Pong) => {}
                Some(Incoming::Text(text)) => {
                    if let Some(message) = Message::parse(&text) {
                        return Ok(Some(message));
                    }
                    debug!("ignored an unrecognised Codex app-server message");
                }
            }
        }
    }

    async fn interrupted(
        &mut self,
        turn_id: &TurnId,
        emit: &mut impl FnMut(Event),
    ) -> Result<TurnOutcome, AppServerError> {
        emit(Event::TurnInterrupted {
            session_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
        });
        self.stop().await?;
        Ok(TurnOutcome::Cancelled)
    }

    async fn crashed(
        &mut self,
        emit: &mut impl FnMut(Event),
    ) -> Result<TurnOutcome, AppServerError> {
        let exit_code = self
            .stop_after(EXIT_GRACE)
            .await?
            .and_then(|status| status.code());
        emit(Event::BackendCrashed {
            session_id: self.session_id.clone(),
            exit_code,
        });
        Ok(TurnOutcome::Crashed { exit_code })
    }

    async fn stop(&mut self) -> std::io::Result<()> {
        self.stop_after(Duration::ZERO).await.map(|_| ())
    }

    /// Gives the child `grace` to exit on its own, then kills its whole group and reaps it.
    async fn stop_after(&mut self, grace: Duration) -> std::io::Result<Option<ExitStatus>> {
        self.closed = true;
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

impl Drop for CodexAppServerSession {
    fn drop(&mut self) {
        // `kill_on_drop` only reaches the leader; descendants need the group signal.
        if let Some(pid) = self.child.as_ref().and_then(Child::id) {
            kill_group(pid);
        }
    }
}

/// Kills the group of a child whose start failed before a session owned it;
/// cleared once the session takes over.
struct ChildGuard(Option<u32>);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_group(pid);
        }
    }
}

fn spawn_server(spec: &AppServerSpec, socket: &Path) -> Result<Child, AppServerError> {
    let mut command = Command::new(&spec.binary);
    command
        .args(LISTEN_ARGS)
        .arg(format!("unix://{}", socket.display()))
        .env_clear()
        .envs(allowlisted(&spec.parent_env))
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    command.spawn().map_err(|source| AppServerError::Spawn {
        path: spec.binary.clone(),
        source,
    })
}

/// Connects once the server has created its socket, giving up as soon as the child
/// has exited; the caller bounds the wait otherwise.
async fn connect(socket: &Path, child: &mut Child) -> Result<UnixStream, AppServerError> {
    loop {
        if let Ok(stream) = UnixStream::connect(socket).await {
            return Ok(stream);
        }
        if let Some(status) = child.try_wait()? {
            return Err(AppServerError::ExitedEarly {
                exit_code: status.code(),
            });
        }
        sleep(CONNECT_POLL).await;
    }
}

async fn upgrade(stream: &mut UnixStream, reader: &mut FrameReader) -> Result<(), AppServerError> {
    stream.write_all(upgrade_request().as_bytes()).await?;
    let mut head = Vec::new();
    let mut chunk = vec![0_u8; MAX_HEAD_BYTES];
    loop {
        if let Some((status, rest)) = parse_upgrade_response(&head)? {
            if status != UPGRADED {
                return Err(AppServerError::Upgrade { status });
            }
            reader.feed(rest);
            return Ok(());
        }
        match stream.read(&mut chunk).await? {
            0 => return Err(AppServerError::ClosedEarly),
            read => head.extend_from_slice(&chunk[..read]),
        }
    }
}

/// Sends a request and waits for its response, answering pings; other messages are skipped.
async fn request(
    stream: &mut UnixStream,
    reader: &mut FrameReader,
    next_id: &mut u64,
    method: &'static str,
    params: Value,
) -> Result<Value, AppServerError> {
    let id = *next_id;
    *next_id += 1;
    let body = json!({"id": id, "method": method, "params": params});
    stream.write_all(&text_frame(&body.to_string())).await?;
    loop {
        match read_incoming(stream, reader).await? {
            None | Some(Incoming::Close) => return Err(AppServerError::ClosedEarly),
            Some(Incoming::Ping(payload)) => stream.write_all(&pong_frame(&payload)).await?,
            Some(Incoming::Pong) => {}
            Some(Incoming::Text(text)) => {
                if let Some(Message::Response {
                    id: got,
                    failed,
                    result,
                }) = Message::parse(&text)
                    && got == json!(id)
                {
                    return if failed {
                        Err(AppServerError::Rejected { method })
                    } else {
                        Ok(result)
                    };
                }
            }
        }
    }
}

/// Reads until one complete frame is available; `None` means the peer closed the stream.
async fn read_incoming(
    stream: &mut UnixStream,
    reader: &mut FrameReader,
) -> Result<Option<Incoming>, AppServerError> {
    let mut buffer = vec![0_u8; READ_CHUNK_BYTES];
    loop {
        if let Some(incoming) = reader.next()? {
            return Ok(Some(incoming));
        }
        match stream.read(&mut buffer).await? {
            0 => return Ok(None),
            read => reader.feed(&buffer[..read]),
        }
    }
}

/// True when the request gives the user nothing to judge: no command, or no path.
fn shows_nothing(request: &ToolRequest) -> bool {
    match request.tool {
        Tool::Bash => request.command.is_none(),
        _ => request.target_paths.is_empty(),
    }
}

fn usage_of(usage: &Value) -> TokenUsage {
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    TokenUsage {
        input_tokens: count("inputTokens"),
        output_tokens: count("outputTokens"),
        cache_read_tokens: count("cachedInputTokens"),
        cache_write_tokens: 0,
    }
}
