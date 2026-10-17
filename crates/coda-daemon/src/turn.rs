// SPDX-License-Identifier: AGPL-3.0-only

//! Runs turns: `session.send`, `session.cancel` and `permission.respond`.
//!
//! A send starts one backend child, records the user message, and returns the
//! turn id. The turn then runs on its own task: events go to the hub, the
//! watchdog aborts a silent backend, and the child is stopped and its pidfile
//! removed however the turn ends.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use coda_config::config::PermissionMode;
use coda_protocol::events::Event;
use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR};
use coda_protocol::methods::{
    PermissionRespondParams, PermissionRespondResult, SessionCancelParams, SessionCancelResult,
    SessionSendParams, SessionSendResult,
};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId, TurnId};
use coda_providers::claude_session::{ClaudeSession, TurnOutcome};
use coda_providers::gate::PermissionGate;
use coda_providers::tool_request::ToolRequest;
use coda_store::{
    EventRecord, LogEvent, LogEventKind, LogScope, PendingPermission, Store, StoreError,
    UsageRecord,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{Instrument, Span, info, warn};

use crate::backends::{SessionStart, StartError, pidfile_path, start_claude_session};
use crate::dispatch::{HandlerResult, error, parse_params, to_value};
use crate::gate::{GateConfig, PermissionAnswers, SessionGate};
use crate::subscribe::{Hub, HubError};
use crate::trace::{backend_span, permission_span};
use crate::watchdog::{Phase, Timing, Watchdog};

/// The session named in a request does not exist.
pub const SESSION_NOT_FOUND: i64 = -32002;
/// The session already has a turn running.
pub const TURN_RUNNING: i64 = -32003;
/// The session uses a backend this daemon cannot run.
pub const BACKEND_UNSUPPORTED: i64 = -32004;

/// Stored event kind of the user's message.
pub const USER_MESSAGE_KIND: &str = "user_message";
const BACKEND_NAME: &str = "claude_code";
/// Events buffered between the adapter and the store writer. The adapter's
/// emit callback is synchronous and must not block, so when the store falls
/// this far behind, further events are dropped with a warning.
const EVENT_QUEUE_CAPACITY: usize = 1024;

/// How turns start their backend.
#[derive(Debug, Clone)]
pub struct TurnConfig {
    /// The resolved Claude Code executable.
    pub binary: PathBuf,
    /// Directory that holds the children's pidfiles.
    pub pidfile_dir: PathBuf,
    /// The environment the child may inherit from, after its allowlist.
    pub parent_env: Vec<(OsString, OsString)>,
    pub timing: Timing,
}

/// Why a request could not be served.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    #[error("session {0} does not exist")]
    SessionNotFound(SessionId),
    #[error("session {0} already has a turn running")]
    Busy(SessionId),
    #[error("session {0} does not use a backend this daemon can run")]
    Unsupported(SessionId),
    #[error(transparent)]
    Hub(#[from] HubError),
    #[error(transparent)]
    Start(#[from] StartError),
}

impl TurnError {
    fn into_rpc(self) -> ErrorObject {
        match self {
            Self::SessionNotFound(_) => error(SESSION_NOT_FOUND, "Session not found"),
            Self::Busy(_) => error(TURN_RUNNING, "A turn is already running"),
            Self::Unsupported(_) => error(BACKEND_UNSUPPORTED, "Backend not supported"),
            other => {
                tracing::error!(error = %other, "request failed");
                error(INTERNAL_ERROR, "Internal error")
            }
        }
    }
}

type CancelHandle = coda_providers::claude_session::CancelHandle;

/// A session's entry in the running table. It is removed on drop, so a turn
/// task that panics or fails during setup never leaves the session busy.
struct TurnSlot {
    turns: Arc<Turns>,
    session: SessionId,
}

impl TurnSlot {
    fn arm(&self, cancel: CancelHandle) {
        self.turns
            .running()
            .insert(self.session.clone(), Some(cancel));
    }
}

impl Drop for TurnSlot {
    fn drop(&mut self) {
        self.turns.running().remove(&self.session);
    }
}

/// The turns of every session the daemon serves.
pub struct Turns {
    hub: Arc<Hub>,
    answers: Arc<PermissionAnswers>,
    config: TurnConfig,
    /// A session is present from the moment a send reserves it; the cancel
    /// handle is set once the backend runs. Never held across an await.
    running: Mutex<HashMap<SessionId, Option<CancelHandle>>>,
    /// Claude Code's own session ids, so the next turn resumes the conversation.
    backend_ids: Mutex<HashMap<SessionId, String>>,
}

struct Job {
    session: SessionId,
    turn: TurnId,
    text: String,
    child: ClaudeSession,
    gate: SessionGate,
}

impl Turns {
    #[must_use]
    pub fn new(hub: Arc<Hub>, config: TurnConfig) -> Arc<Self> {
        Arc::new(Self {
            hub,
            answers: PermissionAnswers::new(),
            config,
            running: Mutex::default(),
            backend_ids: Mutex::default(),
        })
    }

    /// Starts a turn with `text` and returns without waiting for it to end.
    ///
    /// # Errors
    /// Returns [`TurnError::SessionNotFound`], [`TurnError::Unsupported`],
    /// [`TurnError::Busy`], [`TurnError::Start`] when the backend cannot
    /// start, and [`TurnError::Hub`] when the store refuses.
    pub async fn send(self: &Arc<Self>, params: SessionSendParams) -> Result<TurnId, TurnError> {
        let SessionSendParams { session_id, text } = params;
        let id = session_id.clone();
        let record = self
            .hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, &session_id))?;
        if record.backend != BackendKind::ClaudeCode {
            return Err(TurnError::Unsupported(session_id));
        }
        let slot = self.reserve(&session_id)?;
        info!(session = %session_id, step = "start_backend", "starting turn");
        let mut child = start_claude_session(SessionStart {
            binary: self.config.binary.clone(),
            cwd: PathBuf::from(&record.cwd),
            session_id: session_id.clone(),
            resume: self.resume_id(&session_id),
            parent_env: self.config.parent_env.clone(),
            pidfile_dir: self.config.pidfile_dir.clone(),
        })
        .await?;
        let turn = match self.begin(&session_id, &text).await {
            Ok(turn) => turn,
            Err(e) => {
                stop(&mut child, &session_id).await;
                remove_pidfile(&self.config.pidfile_dir, &session_id);
                return Err(e.into());
            }
        };
        let cancel = child.cancel_handle();
        let gate = SessionGate::new(
            Arc::clone(&self.hub),
            GateConfig {
                session: session_id.clone(),
                turn: turn.clone(),
                repo: PathBuf::from(&record.cwd),
                workspace_root: PathBuf::from(&record.cwd),
                mode: permission_mode(record.mode),
                prompt_timeout: self.config.timing.permission,
            },
            Arc::clone(&self.answers),
        );
        slot.arm(cancel);
        let job = Job {
            session: session_id,
            turn: turn.clone(),
            text,
            child,
            gate,
        };
        let turns = Arc::clone(self);
        let span = backend_span(&job.session, &job.turn, BACKEND_NAME);
        spawn_guarded(slot, async move { turns.run(job).await }.instrument(span));
        Ok(turn)
    }

    /// Cancels the running turn of `session`; `false` when none is running.
    /// A turn whose backend is still starting cannot be cancelled yet.
    pub fn cancel(&self, session: &SessionId) -> bool {
        match self.running().get(session) {
            Some(Some(cancel)) => {
                cancel.cancel();
                true
            }
            _ => false,
        }
    }

    /// Hands a client's answer to the gate waiting for it; `false` when the
    /// request is unknown, already answered, or belongs to another session.
    pub async fn respond(&self, params: &PermissionRespondParams) -> bool {
        let session = params.session_id.clone();
        let open = self
            .hub
            .with_store(move |store| store.list_pending(&session))
            .await;
        let owned = open.is_ok_and(|open| open.iter().any(|p| p.id == params.request_id));
        if !owned {
            warn!(
                session = %params.session_id,
                request = %params.request_id,
                "permission answer names no open request of this session"
            );
            return false;
        }
        match self
            .answers
            .answer(&self.hub, &params.request_id, params.decision)
            .await
        {
            Ok(()) => true,
            Err(e) => {
                warn!(request = %params.request_id, error = %e, "permission answer not accepted");
                false
            }
        }
    }

    fn running(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Option<CancelHandle>>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reserve(self: &Arc<Self>, session: &SessionId) -> Result<TurnSlot, TurnError> {
        match self.running().entry(session.clone()) {
            Entry::Occupied(_) => return Err(TurnError::Busy(session.clone())),
            Entry::Vacant(vacant) => vacant.insert(None),
        };
        Ok(TurnSlot {
            turns: Arc::clone(self),
            session: session.clone(),
        })
    }

    fn resume_id(&self, session: &SessionId) -> Option<String> {
        self.backend_ids().get(session).cloned()
    }

    fn backend_ids(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, String>> {
        self.backend_ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    async fn begin(&self, session: &SessionId, text: &str) -> Result<TurnId, HubError> {
        let id = session.clone();
        let turn = self
            .hub
            .with_store_mut(move |store| {
                let turn = store.begin_turn(&id)?;
                store.record_log_event(&log_event(LogEventKind::TurnStarted, &id, &turn))?;
                Ok(turn)
            })
            .await?;
        let body = json!({ "text": text }).to_string();
        let record = EventRecord {
            kind: USER_MESSAGE_KIND.to_owned(),
            body,
        };
        self.hub.publish(session, &turn, record).await?;
        Ok(turn)
    }

    /// The named steps of a running turn: drive the backend, drain its events,
    /// clean up, record the end.
    async fn run(self: Arc<Self>, job: Job) {
        let Job {
            session,
            turn,
            text,
            mut child,
            gate,
        } = job;
        let (tx, rx) = mpsc::channel(EVENT_QUEUE_CAPACITY);
        let publisher = tokio::spawn(publish_events(
            Arc::clone(&self.hub),
            session.clone(),
            turn.clone(),
            rx,
        ));
        let watchdog = Arc::new(Watchdog::new(self.config.timing));
        let watched = WatchedGate {
            inner: gate,
            watchdog: Arc::clone(&watchdog),
            session: session.clone(),
        };
        let observer = Arc::clone(&watchdog);
        let emit_session = session.clone();
        let mut emit = move |event: Event| {
            observer.observe(&event);
            if tx.try_send(event).is_err() {
                warn!(session = %emit_session, "event queue full or closed; dropping an event");
            }
        };
        info!(session = %session, turn = %turn, step = "drive", "driving the backend");
        let mut timed_out = false;
        let outcome = tokio::select! {
            outcome = child.run_turn(&turn, &text, &watched, &mut emit) => outcome,
            phase = watchdog.expired() => {
                warn!(session = %session, turn = %turn, ?phase, "backend silent for too long; aborting");
                timed_out = true;
                emit(Event::TurnInterrupted { session_id: session.clone(), turn_id: turn.clone() });
                Ok(TurnOutcome::Cancelled)
            }
        };
        info!(session = %session, turn = %turn, step = "cleanup", "stopping the backend");
        stop(&mut child, &session).await;
        if let Some(backend_id) = child.session_id_from_backend() {
            self.backend_ids()
                .insert(session.clone(), backend_id.to_owned());
        }
        // Dropping the emitter closes the channel, which ends the publisher.
        drop(emit);
        if publisher.await.is_err() {
            warn!(session = %session, "the event publisher stopped early");
        }
        remove_pidfile(&self.config.pidfile_dir, &session);
        if let Err(e) = &outcome {
            warn!(session = %session, error = %e, "the turn failed");
        }
        // A cancelled turn is the user's choice, not a failure; a watchdog abort is one.
        let failed = timed_out
            || match &outcome {
                Ok(TurnOutcome::Completed { is_error }) => *is_error,
                Ok(TurnOutcome::Cancelled) => false,
                _ => true,
            };
        self.finish(&session, &turn, failed).await;
    }

    async fn finish(&self, session: &SessionId, turn: &TurnId, failed: bool) {
        info!(session = %session, turn = %turn, step = "finish", "recording the end of the turn");
        let (id, turn_id) = (session.clone(), turn.clone());
        let recorded = self
            .hub
            .with_store_mut(move |store| {
                // A prompt that was waiting when the turn ended can never be answered.
                match store.list_pending(&id) {
                    Ok(open) => deny_unanswered(store, &open),
                    Err(e) => warn!(session = %id, error = %e, "could not list open prompts"),
                }
                store.record_log_event(&log_event(LogEventKind::TurnEnded, &id, &turn_id))?;
                if failed {
                    store.record_log_event(&log_event(LogEventKind::Error, &id, &turn_id))?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = recorded {
            warn!(session = %session, error = %e, "could not record the end of the turn");
        }
    }
}

/// Runs `work` on its own task, holding `slot` until it ends or panics.
fn spawn_guarded(
    slot: TurnSlot,
    work: impl Future<Output = ()> + Send + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let _slot = slot;
        work.await;
    })
}

/// Denies each of `open`. A client may have answered one since it was listed;
/// that and any other failure must not stop the rest or the turn's end record.
fn deny_unanswered(store: &mut Store, open: &[PendingPermission]) {
    for pending in open {
        match store.resolve_pending(&pending.id, PermissionDecision::Deny) {
            Ok(())
            | Err(StoreError::AlreadyResolved { .. } | StoreError::PendingNotFound { .. }) => {}
            Err(e) => warn!(request = %pending.id, error = %e, "could not deny an open prompt"),
        }
    }
}

async fn stop(child: &mut ClaudeSession, session: &SessionId) {
    if let Err(e) = child.shutdown().await {
        warn!(session = %session, error = %e, "could not reap the backend");
    }
}

/// Counts a permission prompt as the user's time, not the backend's.
struct WatchedGate {
    inner: SessionGate,
    watchdog: Arc<Watchdog>,
    session: SessionId,
}

impl PermissionGate for WatchedGate {
    async fn request(&self, request: &ToolRequest) -> PermissionDecision {
        self.watchdog.set(Phase::PermissionPending);
        let span = permission_span(&self.session, request.tool.name());
        let decision = self.inner.request(request).instrument(span.clone()).await;
        record_decision(&span, decision);
        decision
    }
}

fn record_decision(span: &Span, decision: PermissionDecision) {
    span.record(
        coda_telemetry::attrs::CODA_PERMISSION_DECISION,
        format!("{decision:?}"),
    );
}

async fn publish_events(
    hub: Arc<Hub>,
    session: SessionId,
    turn: TurnId,
    mut events: mpsc::Receiver<Event>,
) {
    while let Some(event) = events.recv().await {
        if let Err(e) = store_event(&hub, &session, &turn, &event).await {
            warn!(session = %session, error = %e, "could not store an event");
        }
    }
}

async fn store_event(
    hub: &Arc<Hub>,
    session: &SessionId,
    turn: &TurnId,
    event: &Event,
) -> Result<(), HubError> {
    let value = serde_json::to_value(event).unwrap_or(Value::Null);
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    hub.publish(
        session,
        turn,
        EventRecord {
            kind,
            body: value.to_string(),
        },
    )
    .await?;
    if let Event::Usage { usage, .. } = event {
        let record = UsageRecord {
            session: session.clone(),
            turn: turn.clone(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        };
        hub.with_store_mut(move |store| store.record_usage(&record))
            .await?;
    }
    Ok(())
}

fn remove_pidfile(dir: &std::path::Path, session: &SessionId) {
    let path = pidfile_path(dir, session);
    if let Err(e) = std::fs::remove_file(&path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(session = %session, error = %e, "could not remove the pidfile");
    }
}

fn log_event(kind: LogEventKind, session: &SessionId, turn: &TurnId) -> LogEvent {
    LogEvent {
        kind,
        session: session.as_str().parse().ok(),
        turn: turn.as_str().parse().ok(),
        scope: Some(LogScope::Session),
        producer_id: None,
    }
}

fn permission_mode(mode: Mode) -> PermissionMode {
    match mode {
        Mode::Plan => PermissionMode::DenyAll,
        Mode::AcceptEdits => PermissionMode::AcceptEdits,
        _ => PermissionMode::Prompt,
    }
}

fn not_found_or(e: HubError, session: &SessionId) -> TurnError {
    match e {
        HubError::Store(StoreError::SessionNotFound { .. }) => {
            TurnError::SessionNotFound(session.clone())
        }
        other => TurnError::Hub(other),
    }
}

/// Answers `session.send`.
pub(crate) async fn send(turns: Option<&Arc<Turns>>, params: Option<Value>) -> HandlerResult {
    let turns = turns.ok_or_else(unavailable)?;
    let turn_id = turns
        .send(parse_params(params)?)
        .await
        .map_err(TurnError::into_rpc)?;
    to_value(&SessionSendResult { turn_id })
}

/// Answers `session.cancel`.
pub(crate) fn cancel(turns: Option<&Arc<Turns>>, params: Option<Value>) -> HandlerResult {
    let turns = turns.ok_or_else(unavailable)?;
    let params: SessionCancelParams = parse_params(params)?;
    to_value(&SessionCancelResult {
        cancelled: turns.cancel(&params.session_id),
    })
}

/// Answers `permission.respond`.
pub(crate) async fn respond(turns: Option<&Arc<Turns>>, params: Option<Value>) -> HandlerResult {
    let turns = turns.ok_or_else(unavailable)?;
    let params: PermissionRespondParams = parse_params(params)?;
    to_value(&PermissionRespondResult {
        accepted: turns.respond(&params).await,
    })
}

fn unavailable() -> ErrorObject {
    error(coda_protocol::jsonrpc::METHOD_NOT_FOUND, "Method not found")
}

#[cfg(all(test, unix))]
mod tests;
