// SPDX-License-Identifier: AGPL-3.0-only

//! Runs turns: `session.send`, `session.cancel` and `permission.respond`.
//!
//! A send starts one backend child, records the user message, and returns the
//! turn id. The turn then runs on its own task: events go to the hub, the
//! watchdog aborts a silent backend, and the child is stopped and its pidfile
//! removed however the turn ends.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use coda_config::config::{Config, PermissionMode};
use coda_protocol::events::Event;
use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR};
use coda_protocol::methods::{
    PermissionRespondParams, PermissionRespondResult, SessionCancelParams, SessionCancelResult,
    SessionSendParams, SessionSendResult,
};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId, TurnId};
use coda_providers::claude_session::TurnOutcome;
use coda_providers::gate::PermissionGate;
use coda_providers::registry::RegistryError;
use coda_providers::tool_request::ToolRequest;
use coda_store::{
    EventRecord, LogEvent, LogEventKind, LogScope, PendingPermission, SessionRecord, Store,
    StoreError, UsageRecord,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{Instrument, info, warn};

use crate::backends::{
    AgentSession, CodexStart, SessionStart, StartError, pidfile_path, resolve_codex_off_worker,
    start_claude_session, start_codex_session,
};
mod drive;

use self::drive::Drive;
use crate::dispatch::{HandlerResult, error, parse_params, to_value};
use crate::gate::{BrokerGate, Brokers};
use crate::model_session::{Limits, ModelError, ModelSessions};
use crate::subscribe::{Hub, HubError};
use crate::supervisor::{RestartPolicy, Supervisor, TokioSleeper, Verdict};
use crate::trace::backend_span;
use crate::watchdog::{Phase, Timing, Watchdog};

/// The session named in a request does not exist.
pub const SESSION_NOT_FOUND: i64 = -32002;
/// The session already has a turn running.
pub const TURN_RUNNING: i64 = -32003;
/// The session uses a backend this daemon cannot run.
pub const BACKEND_UNSUPPORTED: i64 = -32004;

/// Stored event kind of the user's message.
pub const USER_MESSAGE_KIND: &str = "user_message";
const CLAUDE_BACKEND_NAME: &str = "claude_code";
const CODEX_BACKEND_NAME: &str = "codex";
/// Events buffered between the adapter and the store writer. The adapter's
/// emit callback is synchronous and must not block, so when the store falls
/// this far behind, further events are dropped with a warning.
const EVENT_QUEUE_CAPACITY: usize = 1024;

/// How turns start their backend.
#[derive(Debug, Clone)]
pub struct TurnConfig {
    /// The resolved Claude Code executable, when one was found. Codex is
    /// located per turn from `models` and the `PATH` of `parent_env`.
    pub binary: Option<PathBuf>,
    /// Directory that holds the children's pidfiles.
    pub pidfile_dir: PathBuf,
    /// The environment the child may inherit from, after its allowlist.
    pub parent_env: Vec<(OsString, OsString)>,
    pub timing: Timing,
    /// The configuration model-API sessions select their provider from.
    pub models: Config,
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
    #[error(transparent)]
    Model(#[from] ModelError),
}

impl TurnError {
    pub(crate) fn into_rpc(self) -> ErrorObject {
        match self {
            Self::SessionNotFound(_) => error(SESSION_NOT_FOUND, "Session not found"),
            Self::Busy(_) => error(TURN_RUNNING, "A turn is already running"),
            Self::Unsupported(_)
            | Self::Model(
                ModelError::Disabled(_) | ModelError::Registry(RegistryError::Disabled(_)),
            ) => error(BACKEND_UNSUPPORTED, "Backend not supported"),
            Self::Model(ModelError::Registry(e)) => crate::registry::registry_error(&e),
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
    brokers: Arc<Brokers>,
    config: TurnConfig,
    pub(crate) models: Arc<ModelSessions>,
    /// A session is present from the moment a send reserves it; the cancel
    /// handle is set once the backend runs. Never held across an await.
    running: Mutex<HashMap<SessionId, Option<CancelHandle>>>,
    /// The backends' own session ids, so the next turn resumes the conversation.
    backend_ids: Mutex<HashMap<SessionId, String>>,
    /// Sessions that have run an app-server turn, whose next thread starts empty.
    codex_threads: Mutex<HashSet<SessionId>>,
    supervisor: Supervisor,
    /// Set once the daemon shuts down, so no crashed backend is restarted.
    stopping: AtomicBool,
}

struct Job {
    record: SessionRecord,
    session: SessionId,
    turn: TurnId,
    text: String,
    child: AgentSession,
    gate: BrokerGate,
}

impl Turns {
    /// Turns that restart a crashed backend with the default policy.
    #[must_use]
    pub fn new(hub: Arc<Hub>, config: TurnConfig) -> Arc<Self> {
        let policy = RestartPolicy::new(coda_config::timing::Timing::default().restart_backoff);
        Self::with_supervisor(hub, config, Supervisor::new(policy, Arc::new(TokioSleeper)))
    }

    /// Turns that restart a crashed backend as `supervisor` decides.
    #[must_use]
    pub fn with_supervisor(hub: Arc<Hub>, config: TurnConfig, supervisor: Supervisor) -> Arc<Self> {
        let brokers = Brokers::new(Arc::clone(&hub), config.models.permissions.grants.clone());
        let models = ModelSessions::new(
            Arc::clone(&hub),
            Arc::clone(&brokers),
            config.models.clone(),
            config.parent_env.clone(),
            Arc::new(|name| std::env::var(name).ok()),
        );
        Arc::new(Self {
            hub,
            brokers,
            config,
            models,
            running: Mutex::default(),
            backend_ids: Mutex::default(),
            codex_threads: Mutex::default(),
            supervisor,
            stopping: AtomicBool::new(false),
        })
    }

    /// Starts a turn with `text` and returns without waiting for it to end.
    ///
    /// # Errors
    /// Returns [`TurnError::SessionNotFound`], [`TurnError::Unsupported`],
    /// [`TurnError::Busy`], [`TurnError::Start`] when the backend cannot
    /// start, and [`TurnError::Hub`] when the store refuses.
    pub async fn send(self: &Arc<Self>, params: SessionSendParams) -> Result<TurnId, TurnError> {
        let SessionSendParams {
            session_id,
            text,
            max_turns,
            token_cap,
        } = params;
        let id = session_id.clone();
        let record = self
            .hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, &session_id))?;
        match record.backend {
            BackendKind::ClaudeCode | BackendKind::CodexAppServer | BackendKind::CodexExec => {
                self.send_agent(record, text).await
            }
            BackendKind::Anthropic | BackendKind::OpenaiCompat | BackendKind::Faux => {
                let limits = Limits {
                    max_turns,
                    token_cap,
                };
                self.models.send(record, text, limits).await
            }
            _ => Err(TurnError::Unsupported(session_id)),
        }
    }

    async fn send_agent(
        self: &Arc<Self>,
        record: SessionRecord,
        text: String,
    ) -> Result<TurnId, TurnError> {
        let session_id = record.id.clone();
        let is_claude = record.backend == BackendKind::ClaudeCode;
        if is_claude && self.config.binary.is_none() {
            return Err(TurnError::Unsupported(session_id));
        }
        let slot = self.reserve(&session_id)?;
        let broker = self
            .brokers
            .broker(&record, self.config.timing.permission)
            .await
            .map_err(ModelError::from)?;
        info!(session = %session_id, step = "start_backend", "starting turn");
        let backend_name = if is_claude {
            CLAUDE_BACKEND_NAME
        } else {
            CODEX_BACKEND_NAME
        };
        let mut child = self.start_child(&record).await?;
        let turn = match Self::begin(&self.hub, &session_id, &text).await {
            Ok(turn) => turn,
            Err(e) => {
                stop(&mut child, &session_id).await;
                remove_pidfile(&self.config.pidfile_dir, &session_id);
                return Err(e.into());
            }
        };
        let cancel = child.cancel_handle();
        let gate = BrokerGate::new(
            &self.hub,
            broker,
            &record,
            Arc::new(Mutex::new(turn.clone())),
        );
        slot.arm(cancel);
        let job = Job {
            record,
            session: session_id,
            turn: turn.clone(),
            text,
            child,
            gate,
        };
        let turns = Arc::clone(self);
        let span = backend_span(&job.session, &job.turn, backend_name);
        spawn_guarded(slot, async move { turns.run(job).await }.instrument(span));
        Ok(turn)
    }

    /// Starts the backend child of `record`'s session, with its pidfile.
    async fn start_child(&self, record: &SessionRecord) -> Result<AgentSession, StartError> {
        let session_id = record.id.clone();
        let cwd = PathBuf::from(&record.cwd);
        let resume = self.resume_id(&session_id);
        let parent_env = self.config.parent_env.clone();
        let pidfile_dir = self.config.pidfile_dir.clone();
        if let (BackendKind::ClaudeCode, Some(binary)) = (record.backend, &self.config.binary) {
            let session = start_claude_session(SessionStart {
                binary: binary.clone(),
                cwd,
                session_id,
                resume,
                parent_env,
                pidfile_dir,
            })
            .await?;
            return Ok(AgentSession::Claude(Box::new(session)));
        }
        let search_dirs = path_dirs(&parent_env);
        let settings = resolve_codex_off_worker(self.config.models.clone(), search_dirs).await?;
        let continued = self.codex_threads().contains(&session_id);
        let session = start_codex_session(CodexStart {
            settings,
            cwd,
            session_id: session_id.clone(),
            resume,
            continued,
            parent_env,
            pidfile_dir,
        })
        .await?;
        self.codex_threads().insert(session_id);
        Ok(AgentSession::Codex(Box::new(session)))
    }

    /// Cancels the running turn of `session`; `false` when none is running.
    /// A turn whose backend is still starting cannot be cancelled yet.
    pub fn cancel(&self, session: &SessionId) -> bool {
        match self.running().get(session) {
            Some(Some(cancel)) => {
                cancel.cancel();
                true
            }
            _ => self.models.cancel(session),
        }
    }

    /// Hands a client's answer to the gate waiting for it; `false` when the
    /// request is unknown, already answered, or belongs to another session.
    pub async fn respond(&self, params: &PermissionRespondParams) -> bool {
        self.brokers.respond(params).await
    }

    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.models.shutdown().await;
    }

    /// Whether `session` has a turn starting or running.
    #[must_use]
    pub fn is_running(&self, session: &SessionId) -> bool {
        self.running().contains_key(session) || self.models.is_running(session)
    }

    /// How many sessions have a turn starting or running.
    #[must_use]
    pub fn active(&self) -> u32 {
        let running = self.running().len() + self.models.active();
        u32::try_from(running).unwrap_or(u32::MAX)
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

    fn codex_threads(&self) -> std::sync::MutexGuard<'_, HashSet<SessionId>> {
        self.codex_threads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn backend_ids(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, String>> {
        self.backend_ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a turn and stores the user's message as its first event.
    pub(crate) async fn begin(
        hub: &Arc<Hub>,
        session: &SessionId,
        text: &str,
    ) -> Result<TurnId, HubError> {
        let id = session.clone();
        let turn = hub
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
        if let Err(e) = hub.publish(session, &turn, record).await {
            // No task will run this turn, so nothing else would end it.
            Self::finish(hub, session, &turn, true).await;
            return Err(e);
        }
        Ok(turn)
    }

    /// The named steps of a running turn: drive the backend (restarting it
    /// when it crashes), drain its events, clean up, record the end.
    async fn run(self: Arc<Self>, job: Job) {
        let Job {
            record,
            session,
            turn,
            text,
            child,
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
            tool_requested: AtomicBool::new(false),
        };
        let observer = Arc::clone(&watchdog);
        let emit_session = session.clone();
        let emit = move |event: Event| {
            observer.observe(&event);
            if tx.try_send(event).is_err() {
                warn!(session = %emit_session, "event queue full or closed; dropping an event");
            }
        };
        info!(session = %session, turn = %turn, step = "drive", "driving the backend");
        let mut drive = Drive::new(
            &self,
            &record,
            &turn,
            &text,
            (&watched, &watchdog),
            (child, emit),
        );
        let verdict = self.supervisor.run(&mut drive).await;
        if let Verdict::Exhausted { restarts } = verdict {
            warn!(session = %session, turn = %turn, restarts, "restart cap reached; failing the turn");
            (drive.emit)(Event::Result {
                session_id: session.clone(),
                turn_id: turn.clone(),
                is_error: true,
            });
        }
        let Drive {
            emit,
            mut child,
            outcome,
            timed_out,
            ..
        } = drive;
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
        if let Some(Err(e)) = &outcome {
            warn!(session = %session, error = %e, "the turn failed");
        }
        // A cancelled turn is the user's choice, not a failure; a watchdog abort is one.
        let failed = timed_out
            || match &outcome {
                Some(Ok(TurnOutcome::Completed { is_error })) => *is_error,
                Some(Ok(TurnOutcome::Cancelled)) => false,
                _ => true,
            };
        Self::finish(&self.hub, &session, &turn, failed).await;
    }

    /// Denies the prompts the turn left open and records how it ended.
    pub(crate) async fn finish(hub: &Arc<Hub>, session: &SessionId, turn: &TurnId, failed: bool) {
        info!(session = %session, turn = %turn, step = "finish", "recording the end of the turn");
        let (id, turn_id) = (session.clone(), turn.clone());
        let recorded = hub
            .with_store_mut(move |store| {
                // A prompt that was waiting when the turn ended can never be answered.
                match store.list_pending(&id) {
                    Ok(open) => deny_unanswered(store, &open),
                    Err(e) => warn!(session = %id, error = %e, "could not list open prompts"),
                }
                // End the turn first: the log is best effort, and a turn left
                // open would be reported as interrupted at the next start.
                let ended = store.end_turn(&id, &turn_id);
                let logged = store
                    .record_log_event(&log_event(LogEventKind::TurnEnded, &id, &turn_id))
                    .and_then(|()| {
                        if failed {
                            store.record_log_event(&log_event(LogEventKind::Error, &id, &turn_id))
                        } else {
                            Ok(())
                        }
                    });
                ended.and(logged)
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

/// The directories on the `PATH` of `env`.
fn path_dirs(env: &[(OsString, OsString)]) -> Vec<PathBuf> {
    env.iter()
        .find(|(name, _)| name == "PATH")
        .map(|(_, value)| std::env::split_paths(value).collect())
        .unwrap_or_default()
}

async fn stop(child: &mut AgentSession, session: &SessionId) {
    if let Err(e) = child.shutdown().await {
        warn!(session = %session, error = %e, "could not reap the backend");
    }
}

/// Counts a permission prompt as the user's time, not the backend's.
struct WatchedGate {
    inner: BrokerGate,
    watchdog: Arc<Watchdog>,
    /// Set once the backend asks to run any tool, which may have side effects.
    tool_requested: AtomicBool,
}

impl WatchedGate {
    fn tool_requested(&self) -> bool {
        self.tool_requested.load(Ordering::SeqCst)
    }
}

impl PermissionGate for WatchedGate {
    async fn request(&self, request: &ToolRequest) -> PermissionDecision {
        self.tool_requested.store(true, Ordering::SeqCst);
        self.watchdog.set(Phase::PermissionPending);
        self.inner.request(request).await
    }
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

pub(crate) async fn store_event(
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

pub(crate) fn log_event(kind: LogEventKind, session: &SessionId, turn: &TurnId) -> LogEvent {
    LogEvent {
        kind,
        session: session.as_str().parse().ok(),
        turn: turn.as_str().parse().ok(),
        scope: Some(LogScope::Session),
        producer_id: None,
    }
}

pub(crate) fn permission_mode(mode: Mode) -> PermissionMode {
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
