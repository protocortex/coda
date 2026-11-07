// SPDX-License-Identifier: Apache-2.0

//! Runs turns: `session.send`, `session.cancel` and `permission.respond`.
//!
//! A send starts one backend child, records the user message, and returns the
//! turn id. The turn then runs on its own task: events go to the hub, the
//! watchdog aborts a silent backend, and the child is stopped and its pidfile
//! removed however the turn ends.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use coda_config::config::{Config, PermissionMode};
use coda_core::history::Transcript;
use coda_core::turn::{FenceAck, TurnEvent};
use coda_protocol::events::Event;
use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR, INVALID_PARAMS};
use coda_protocol::methods::{
    AgentRunParams, AgentRunResult, PermissionRespondParams, PermissionRespondResult,
    REWIND_REFUSED_CODE, SessionCancelParams, SessionCancelResult, SessionCloseParams,
    SessionCloseResult, SessionRewindParams, SessionSendParams, SessionSendResult,
};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId, TurnId};
use coda_providers::claude_session::TurnOutcome;
use coda_providers::codex::Carried;
use coda_providers::codex_exec::SandboxMode;
use coda_providers::detect::DEFAULT_PROBE_TIMEOUT;
use coda_providers::emit::Emit;
use coda_providers::gate::PermissionGate;
use coda_providers::registry::RegistryError;
use coda_providers::tool_request::ToolRequest;
use coda_store::{
    EventRecord, LogEvent, LogEventKind, LogScope, PendingPermission, SessionRecord, Store,
    StoreError, UsageRecord,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{Instrument, info, warn};

use crate::backends::{
    AgentSession, CodexStart, SessionStart, StartError, pidfile_path, resolve_codex_off_worker,
    start_claude_session, start_codex_session,
};
mod drive;
mod rewind;

use self::drive::Drive;
use crate::checkpoint;
use crate::dispatch::{HandlerResult, error, parse_params, to_value};
use crate::gate::{BrokerGate, Brokers};
use crate::idle_pool::{IdlePool, MAX_IDLE_SERVERS};
use crate::model_session::{Limits, ModelError, ModelSessions};
use crate::pidfile;
use crate::subscribe::{Hub, HubError};
use crate::supervisor::{RestartPolicy, Supervisor, TokioSleeper, Verdict};
use crate::trace::backend_span;
use crate::watchdog::{Phase, Timing, Watchdog};

/// The session named in a request does not exist.
pub const SESSION_NOT_FOUND: i64 = -32002;
/// The session already has a turn running.
pub const TURN_RUNNING: i64 = coda_protocol::methods::TURN_RUNNING_CODE;
/// The session uses a backend this daemon cannot run.
pub const BACKEND_UNSUPPORTED: i64 = -32004;

pub use coda_core::history::USER_MESSAGE_KIND;
/// How long a model-API fence waits for its answer. The loop sits idle between
/// turns with its channel open, so a fence that arrives after the last read
/// would otherwise wait for the next turn.
pub(crate) const FENCE_TIMEOUT: Duration = Duration::from_secs(5);

const AGENT_NEEDS_DELEGATED: &str = "agent.run needs a Claude Code or Codex session; a model API session runs agents through its task tool";
const AGENT_NEEDS_PLAN_MODE: &str =
    "agent.run needs a session in plan mode, so the agent cannot change anything";
/// Between an agent's instructions and the request it is given.
const AGENT_REQUEST_HEADING: &str = "The request:";
const CLAUDE_BACKEND_NAME: &str = "claude_code";
const CODEX_BACKEND_NAME: &str = "codex";
/// How long the publisher gathers consecutive text deltas into one stored chunk.
pub(crate) const DELTA_WINDOW: Duration = Duration::from_millis(50);
/// Size at which a stored text chunk stops growing, so a long stream is still stored
/// in pieces. The last delta may take a chunk a little past it.
pub(crate) const MAX_DELTA_CHUNK_BYTES: usize = 16 * 1024;
/// How long a request waits for a session whose turn is ending: its terminal
/// event is out, or it was cancelled and has not ended yet. Stopping a backend
/// after the cancel or the event can take a few seconds.
const WIND_DOWN_TIMEOUT: Duration = Duration::from_secs(5);
const WIND_DOWN_POLL: Duration = Duration::from_millis(10);
/// Events a turn may have emitted that the publisher has not stored yet. The
/// backend waits when the queue holds this many.
const EVENT_QUEUE_BOUND: usize = 1024;
/// Bytes of earlier conversation given to a Codex thread that replaces a lost one.
const MAX_SEED_BYTES: usize = 32 * 1024;
/// Why a fresh Claude Code conversation has none of the earlier turns.
const HISTORY_UNREAD_REASON: &str = "the stored conversation could not be read";

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
    #[error("session {0} is still ending a cancelled turn")]
    Ending(SessionId),
    #[error("session {0} does not use a backend this daemon can run")]
    Unsupported(SessionId),
    /// A `session.rewind` that cannot be done as asked. The text is for the caller.
    #[error("{0}")]
    Rewind(String),
    /// An `agent.run` request that cannot be served as asked. The text is for the caller.
    #[error("{0}")]
    Agent(String),
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
            Self::Agent(why) => error(INVALID_PARAMS, &why),
            Self::Rewind(why) => error(REWIND_REFUSED_CODE, &why),
            Self::Busy(_) => error(TURN_RUNNING, "A turn is already running"),
            Self::Ending(_) => error(
                TURN_RUNNING,
                "The cancelled turn is still ending. Try again.",
            ),
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

/// A session's place in a running table from the moment a send reserves it.
/// It is armed once the backend can be cancelled; a cancel that arrives earlier
/// is kept and applied by [`Reservation::arm`]. The reservation leaves the
/// table with its turn, so no flag outlives it.
pub(crate) struct Reservation<T> {
    armed: Option<T>,
    cancel: CancelState,
    cancellable: bool,
    /// The turn has emitted its terminal event and only cleans up now.
    winding_down: bool,
}

/// What a session's reservation says about its turn, read under one lock per table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnState {
    /// No turn holds the session.
    Free,
    /// The turn is running, or starting, and nobody cancelled it.
    Live,
    /// The turn is ending: its terminal event is out, or it was cancelled and
    /// the turn task has not yet said so.
    WindingDown { cancelled: bool },
}

/// How far a cancel of a reservation has got.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CancelState {
    /// Nobody cancelled.
    None,
    /// Asked for before the backend could be cancelled; [`Reservation::arm`] applies it.
    Pending,
    /// Handed to the backend, which may not have reacted yet.
    Sent,
}

impl<T> Default for Reservation<T> {
    fn default() -> Self {
        Self {
            armed: None,
            cancel: CancelState::None,
            cancellable: true,
            winding_down: false,
        }
    }
}

impl<T> Reservation<T> {
    /// A reservation of work that has nothing to cancel, which only keeps the
    /// session busy.
    pub(crate) fn uncancellable() -> Self {
        Self {
            cancellable: false,
            ..Self::default()
        }
    }

    /// Cancels now when armed, else remembers the request for [`Self::arm`].
    /// Returns whether there was anything to cancel.
    pub(crate) fn request_cancel(&mut self, cancel: impl FnOnce(&T)) -> bool {
        if !self.cancellable {
            return false;
        }
        match &self.armed {
            Some(armed) => {
                cancel(armed);
                self.cancel = CancelState::Sent;
            }
            None => self.cancel = CancelState::Pending,
        }
        true
    }

    /// Stores `armed` and applies a cancel requested while it was missing.
    pub(crate) fn arm(&mut self, armed: T, cancel: impl FnOnce(&T)) {
        if self.cancel == CancelState::Pending {
            cancel(&armed);
            self.cancel = CancelState::Sent;
        }
        self.armed = Some(armed);
    }

    /// Replaces the armed value of a turn whose backend started again. A cancel
    /// asked for meanwhile already reached the old value, which the caller
    /// hands on.
    pub(crate) fn rearm(&mut self, armed: T) {
        self.armed = Some(armed);
    }

    /// Marks that the turn's terminal event is out, so a request for the
    /// session may wait for the slot instead of being refused.
    pub(crate) fn wind_down(&mut self) {
        self.winding_down = true;
    }

    /// How the turn stands: live, or ending because it emitted its terminal
    /// event or was cancelled.
    pub(crate) fn state(&self) -> TurnState {
        let cancelled = self.cancel != CancelState::None;
        if self.winding_down || cancelled {
            TurnState::WindingDown { cancelled }
        } else {
            TurnState::Live
        }
    }

    pub(crate) fn armed(&self) -> Option<&T> {
        self.armed.as_ref()
    }

    pub(crate) fn into_armed(self) -> Option<T> {
        self.armed
    }
}

/// A session's entry in the running table. It is removed on drop, so a turn
/// task that panics or fails during setup never leaves the session busy.
struct TurnSlot {
    turns: Arc<Turns>,
    session: SessionId,
}

impl TurnSlot {
    fn arm(&self, cancel: CancelHandle) {
        if let Some(reservation) = self.turns.running().get_mut(&self.session) {
            reservation.arm(cancel, CancelHandle::cancel);
        }
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
    running: Mutex<HashMap<SessionId, Reservation<CancelHandle>>>,
    /// The backends' own session ids, so the next turn resumes the conversation.
    backend_ids: Mutex<HashMap<SessionId, String>>,
    /// The event queue of each running turn, to count what is waiting.
    queues: Mutex<HashMap<SessionId, mpsc::WeakSender<Queued>>>,
    /// Codex app-servers between turns, one per session. A turn takes its
    /// session's server out, so turns of one session never share it at once.
    idle: IdlePool<AgentSession>,
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
    /// The kept turns as text, for a conversation that starts new after a rewind.
    seed: Option<String>,
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
            queues: Mutex::default(),
            idle: IdlePool::new(MAX_IDLE_SERVERS),
            supervisor,
            stopping: AtomicBool::new(false),
        })
    }

    /// Starts a turn with `text` and returns without waiting for it to end.
    ///
    /// # Errors
    /// Returns [`TurnError::SessionNotFound`], [`TurnError::Unsupported`],
    /// [`TurnError::Busy`], [`TurnError::Ending`] when a cancelled turn does
    /// not end in time, [`TurnError::Start`] when the backend cannot start,
    /// and [`TurnError::Hub`] when the store refuses.
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
        self.await_wind_down(&session_id).await?;
        match record.backend {
            BackendKind::ClaudeCode | BackendKind::CodexAppServer | BackendKind::CodexExec => {
                self.send_agent(record, text.clone(), text).await
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

    /// Runs the named agent on a prompt in a session of a delegated backend. The
    /// backend is given the agent's instructions, then the prompt. The session must
    /// be in plan mode, where every permission request is denied, so the agent can
    /// read and cannot change anything. Only the prompt is stored as the user's message.
    ///
    /// # Errors
    /// Returns [`TurnError::Agent`] when the backend is a model API, the session is
    /// not in plan mode, or the agent cannot be run, and the errors of
    /// [`Turns::send`] otherwise.
    pub async fn run_agent(self: &Arc<Self>, params: AgentRunParams) -> Result<TurnId, TurnError> {
        let AgentRunParams {
            session_id,
            agent,
            text,
        } = params;
        let id = session_id.clone();
        let record = self
            .hub
            .with_store(move |store| store.load_session(&id))
            .await
            .map_err(|e| not_found_or(e, &session_id))?;
        self.await_wind_down(&session_id).await?;
        if record.backend.is_model_backend() {
            return Err(TurnError::Agent(AGENT_NEEDS_DELEGATED.to_owned()));
        }
        if record.mode != Mode::Plan {
            return Err(TurnError::Agent(AGENT_NEEDS_PLAN_MODE.to_owned()));
        }
        let catalog = self.models.catalog(&record.cwd).await;
        let entry = catalog
            .dispatchable(&agent)
            .map_err(|e| TurnError::Agent(e.to_string()))?;
        let prompt = format!("{}\n\n{AGENT_REQUEST_HEADING}\n\n{text}", entry.def.body);
        self.send_agent(record, text, prompt).await
    }

    /// Starts a turn on a delegated backend. `shown` is stored as the user's
    /// message and `prompt` is what the backend is given.
    async fn send_agent(
        self: &Arc<Self>,
        record: SessionRecord,
        shown: String,
        prompt: String,
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
        let mut child = self.start_child(&record, false).await?;
        let seed = self.rewind_seed(&record).await;
        let turn = match Self::begin(&self.hub, &session_id, &shown).await {
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
            text: prompt,
            child,
            gate,
            seed,
        };
        let turns = Arc::clone(self);
        let span = backend_span(&job.session, &job.turn, backend_name);
        spawn_guarded(slot, async move { turns.run(job).await }.instrument(span));
        Ok(turn)
    }

    /// Starts the backend child of `record`'s session, with its pidfile.
    async fn start_child(
        &self,
        record: &SessionRecord,
        fresh: bool,
    ) -> Result<AgentSession, StartError> {
        let session_id = record.id.clone();
        let cwd = PathBuf::from(&record.cwd);
        let resume = if fresh {
            None
        } else {
            self.resume_id(record).await
        };
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
                read_only: record.mode == Mode::Plan,
            })
            .await?;
            return Ok(AgentSession::Claude(Box::new(session)));
        }
        if let Some(idle) = self.reuse_idle(&session_id).await {
            return Ok(idle);
        }
        let search_dirs = path_dirs(&parent_env);
        let mut settings = resolve_codex_off_worker(
            self.config.models.clone(),
            search_dirs,
            DEFAULT_PROBE_TIMEOUT,
        )
        .await?;
        // A plan-mode session reads and changes nothing, whatever sandbox is configured.
        if record.mode == Mode::Plan {
            settings.sandbox = SandboxMode::ReadOnly;
        }
        let carried = if record.backend == BackendKind::CodexAppServer {
            self.carried(&session_id, None).await
        } else {
            Carried::Nothing
        };
        let session = start_codex_session(CodexStart {
            settings,
            cwd,
            session_id,
            resume,
            carried,
            parent_env,
            pidfile_dir,
        })
        .await?;
        Ok(AgentSession::Codex(Box::new(session)))
    }

    /// The idle server of `session` when it still runs. One that died while idle
    /// is stopped, so the caller starts a single new one.
    async fn reuse_idle(&self, session: &SessionId) -> Option<AgentSession> {
        let mut idle = self.idle.take(session)?;
        if idle.is_reusable() {
            return Some(idle);
        }
        info!(session = %session, "the idle Codex server is gone; starting a new one");
        stop(&mut idle, session).await;
        remove_pidfile(&self.config.pidfile_dir, session);
        None
    }

    /// The stored conversation, for a new thread to pick up where the lost one stopped.
    /// `skip` leaves out a turn that is being sent again.
    async fn carried(&self, session: &SessionId, skip: Option<&TurnId>) -> Carried {
        let mut transcript = Transcript::new(MAX_SEED_BYTES);
        if let Some(turn) = skip {
            transcript = transcript.without_turn(turn.clone());
        }
        let mut after = 0;
        loop {
            let id = session.clone();
            let page = self
                .hub
                .with_store(move |store| {
                    let next = transcript.apply_page(store, &id, after)?;
                    Ok((transcript, next))
                })
                .await;
            match page {
                Ok((back, Some(last))) => {
                    transcript = back;
                    after = last;
                }
                Ok((back, None)) => return back.render().map_or(Carried::Nothing, Carried::Seed),
                Err(error) => {
                    warn!(session = %session, %error, "could not read the conversation for a new Codex thread");
                    return Carried::Lost;
                }
            }
        }
    }

    /// Keeps `child` for the session's next turn. A server the pool pushes out,
    /// or all of them once the daemon stops, is stopped here.
    async fn park(&self, session: &SessionId, child: AgentSession) {
        if let Some((id, mut evicted)) = self.idle.keep(session.clone(), child) {
            info!(session = %id, "stopping an idle Codex server");
            let pid = evicted.pid();
            stop(&mut evicted, &id).await;
            remove_pidfile_of(&self.config.pidfile_dir, &id, pid);
        }
    }

    /// Cancels the running turn of `session`; `false` when none is running.
    /// A turn whose backend is still starting is cancelled as soon as it can be.
    pub fn cancel(&self, session: &SessionId) -> bool {
        match self.running().get_mut(session) {
            Some(reservation) => reservation.request_cancel(CancelHandle::cancel),
            None => self.models.cancel(session),
        }
    }

    /// Cancels like [`Self::cancel`], then kills the backend's process group at
    /// once instead of waiting for it to answer the cancel. For a stop that did not take.
    pub async fn force_cancel(&self, session: &SessionId) -> bool {
        if !self.running().contains_key(session) {
            return self.models.force_cancel(session);
        }
        let cancelled = self.cancel(session);
        if cancelled {
            let path = pidfile_path(&self.config.pidfile_dir, session);
            if crate::reaper::kill_recorded(&path).await {
                info!(session = %session, "killed the backend after a forced cancel");
            }
        }
        cancelled
    }

    /// Releases what the daemon holds for an idle `session`: its pooled backend
    /// server and its idle model conversation. Stored events stay, so the session
    /// can be reopened and sent to again. Returns whether anything was released;
    /// an unknown or already closed session is not an error.
    ///
    /// A turn is never cancelled here, so a caller that wants the session closed
    /// cancels it first; the close then waits for that turn to end.
    ///
    /// # Errors
    /// Returns [`TurnError::Busy`] while a turn is starting or running, and
    /// [`TurnError::Ending`] when a cancelled turn does not end in time.
    pub async fn close(&self, session: &SessionId) -> Result<bool, TurnError> {
        self.await_wind_down(session).await?;
        Ok(self.release(session).await)
    }

    /// Whether any backend is enabled for new sessions.
    #[must_use]
    pub fn has_backends(&self) -> bool {
        !self.models.available_backends().is_empty()
    }

    /// Hands a client's answer to the gate waiting for it; `false` when the
    /// request is unknown, already answered, or belongs to another session.
    pub async fn respond(&self, params: &PermissionRespondParams) -> bool {
        self.brokers.respond(params).await
    }

    pub async fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.models.shutdown().await;
        for (session, mut child) in self.idle.close() {
            stop(&mut child, &session).await;
            remove_pidfile(&self.config.pidfile_dir, &session);
        }
    }

    /// Returns once `session` has no turn. A turn that already emitted its
    /// terminal event is waited for, since a client gets that event before the
    /// session is free, and so is a cancelled one, since a client that cancels
    /// and then closes does not wait for the turn to notice; a live turn is
    /// refused at once.
    ///
    /// # Errors
    /// Returns [`TurnError::Busy`] for a live turn, or one that outlasts
    /// [`WIND_DOWN_TIMEOUT`], and [`TurnError::Ending`] when the one that
    /// outlasts it was cancelled.
    pub(crate) async fn await_wind_down(&self, session: &SessionId) -> Result<(), TurnError> {
        let deadline = tokio::time::Instant::now() + WIND_DOWN_TIMEOUT;
        loop {
            match self.turn_state(session) {
                TurnState::Free => return Ok(()),
                TurnState::Live => return Err(TurnError::Busy(session.clone())),
                TurnState::WindingDown { cancelled } => {
                    if tokio::time::Instant::now() >= deadline {
                        return Err(if cancelled {
                            TurnError::Ending(session.clone())
                        } else {
                            TurnError::Busy(session.clone())
                        });
                    }
                    tokio::time::sleep(WIND_DOWN_POLL).await;
                }
            }
        }
    }

    /// The state of `session`'s turn, with one read of each table so a turn
    /// that ends meanwhile is never taken for a live one.
    fn turn_state(&self, session: &SessionId) -> TurnState {
        let agent = self.running().get(session).map(Reservation::state);
        agent
            .or_else(|| self.models.turn_state(session))
            .unwrap_or(TurnState::Free)
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

    fn running(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Reservation<CancelHandle>>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reserve(self: &Arc<Self>, session: &SessionId) -> Result<TurnSlot, TurnError> {
        match self.running().entry(session.clone()) {
            Entry::Occupied(_) => return Err(TurnError::Busy(session.clone())),
            Entry::Vacant(vacant) => vacant.insert(Reservation::default()),
        };
        Ok(TurnSlot {
            turns: Arc::clone(self),
            session: session.clone(),
        })
    }

    /// The id to resume: the one this daemon saw, else the one stored before it restarted.
    pub(super) async fn resume_id(&self, record: &SessionRecord) -> Option<String> {
        if let Some(id) = self.backend_ids().get(&record.id).cloned() {
            return Some(id);
        }
        if record.backend != BackendKind::ClaudeCode {
            return None;
        }
        let session = record.id.clone();
        let stored = self
            .hub
            .with_store(move |store| store.active_backend_ref(&session, BackendKind::ClaudeCode))
            .await;
        stored.unwrap_or_else(|error| {
            warn!(session = %record.id, %error, "could not read the stored backend id");
            None
        })
    }

    /// Keeps the id the backend reported, in memory and, for Claude, in the store.
    pub(super) async fn remember_backend_id(&self, record: &SessionRecord, backend_id: &str) {
        self.backend_ids()
            .insert(record.id.clone(), backend_id.to_owned());
        if record.backend != BackendKind::ClaudeCode {
            return;
        }
        let (session, backend_id) = (record.id.clone(), backend_id.to_owned());
        let stored = self
            .hub
            .with_store_mut(move |store| {
                store.record_backend_thread(&session, BackendKind::ClaudeCode, &backend_id)
            })
            .await;
        if let Err(error) = stored {
            warn!(session = %record.id, %error, "could not store the backend id");
        }
    }

    /// Stops the daemon from resuming the id it holds in memory. The stored row
    /// stays active: only a conversation the backend names later replaces it.
    pub(super) fn forget_resume_id(&self, record: &SessionRecord) {
        self.backend_ids().remove(&record.id);
    }

    fn queues(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, mpsc::WeakSender<Queued>>> {
        self.queues.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many events of `session`'s running turn wait to be stored.
    #[must_use]
    pub fn queued_events(&self, session: &SessionId) -> usize {
        let queue = self
            .queues()
            .get(session)
            .and_then(mpsc::WeakSender::upgrade);
        queue.map_or(0, |tx| tx.max_capacity() - tx.capacity())
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
        checkpoint::before_turn(hub, session, &turn).await;
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
            seed,
        } = job;
        let (tx, rx) = mpsc::channel(EVENT_QUEUE_BOUND);
        self.queues().insert(session.clone(), tx.downgrade());
        gate.fence().attach(tx.downgrade());
        let mut publisher = tokio::spawn(publish_events(
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
        let emit = EventSink {
            outbox: EventOutbox::new(tx),
            watchdog: Arc::clone(&watchdog),
            turns: Arc::clone(&self),
            session: session.clone(),
        };
        info!(session = %session, turn = %turn, step = "drive", "driving the backend");
        let mut drive = Drive::new(
            &self,
            &record,
            &turn,
            &text,
            (&watched, &watchdog),
            (child, emit),
        )
        .with_seed(seed);
        let verdict = self.supervisor.run(&mut drive).await;
        if let Verdict::Exhausted { restarts } = verdict {
            warn!(session = %session, turn = %turn, restarts, "restart cap reached; failing the turn");
            drive
                .emit
                .emit(Event::Result {
                    session_id: session.clone(),
                    turn_id: turn.clone(),
                    is_error: true,
                })
                .await;
        }
        let Drive {
            emit,
            mut child,
            outcome,
            timed_out,
            ..
        } = drive;
        // A store that never answers must not hold the slot of a turn the watchdog aborted.
        let limit = timed_out.then_some(self.config.timing.store_stall);
        // A turn the watchdog aborted may still run on the server, so it is not kept.
        let keep = !timed_out
            && matches!(
                outcome,
                Some(Ok(TurnOutcome::Completed { .. } | TurnOutcome::Cancelled))
            )
            && child.is_reusable();
        if keep {
            info!(session = %session, turn = %turn, step = "cleanup", "keeping the backend for the next turn");
            self.park(&session, child).await;
        } else {
            info!(session = %session, turn = %turn, step = "cleanup", "stopping the backend");
            stop(&mut child, &session).await;
            if let Some(backend_id) = child.session_id_from_backend() {
                let saved = within(limit, self.remember_backend_id(&record, backend_id)).await;
                if saved.is_none() {
                    warn!(session = %session, "the store did not answer; the backend id was not saved");
                }
            }
        }
        let store_failures = settle_events(&session, emit, &mut publisher, limit).await;
        self.queues().remove(&session);
        if !keep {
            remove_pidfile(&self.config.pidfile_dir, &session);
        }
        if let Some(Err(e)) = &outcome {
            warn!(session = %session, error = %e, "the turn failed");
        }
        let failed = turn_failed(timed_out || store_failures > 0, &outcome);
        let ended = within(limit, Self::finish(&self.hub, &session, &turn, failed)).await;
        if ended.is_none() {
            warn!(session = %session, "the store did not answer; the turn is left open");
        } else {
            checkpoint::after_turn(&self.hub, &session, &turn).await;
        }
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

/// Whether a turn failed. A cancelled turn is the user's choice, not a failure; a
/// watchdog abort is one, and so is an event that could not be stored, which
/// leaves a hole in the log. `hole_or_abort` says either happened.
fn turn_failed(hole_or_abort: bool, outcome: &drive::Outcome) -> bool {
    hole_or_abort
        || match outcome {
            Some(Ok(TurnOutcome::Completed { is_error })) => *is_error,
            Some(Ok(TurnOutcome::Cancelled)) => false,
            _ => true,
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

    fn progress(&self) {
        self.watchdog.touch();
    }

    fn tool_started(&self, id: &str) {
        self.watchdog.tool_started(id);
    }

    fn tool_finished(&self, id: &str) {
        self.watchdog.tool_finished(id);
    }
}

/// What travels to the publisher: an event to store, or a fence.
#[derive(Debug)]
pub(crate) enum Queued {
    Event(Event),
    /// Answered once every event queued before it is stored.
    Fence(oneshot::Sender<()>),
}

/// Lets something outside the event queue wait until the events already
/// queued are stored, so what it writes next lands after them in the log.
#[derive(Default)]
pub(crate) struct Fence {
    queue: OnceLock<Attached>,
}

/// The channel a fence sends its marker through.
enum Attached {
    /// A Claude Code or Codex turn's queue.
    Backend(mpsc::WeakSender<Queued>),
    /// A model-API session's event channel.
    Model(mpsc::WeakUnboundedSender<TurnEvent>),
}

impl Fence {
    pub(crate) fn attach(&self, queue: mpsc::WeakSender<Queued>) {
        // A turn's queue is attached once; a second attach has nothing to add.
        let _ = self.queue.set(Attached::Backend(queue));
    }

    pub(crate) fn attach_model(&self, events: mpsc::WeakUnboundedSender<TurnEvent>) {
        let _ = self.queue.set(Attached::Model(events));
    }

    /// Returns when the queued events are stored, or at once when there is no
    /// queue or its publisher is gone.
    pub(crate) async fn wait(&self) {
        match self.queue.get() {
            Some(Attached::Backend(queue)) => {
                let Some(queue) = queue.upgrade() else {
                    return;
                };
                let (reached, stored) = oneshot::channel();
                if queue.send(Queued::Fence(reached)).await.is_ok() {
                    let _ = stored.await;
                }
            }
            Some(Attached::Model(events)) => {
                let Some(events) = events.upgrade() else {
                    return;
                };
                let (reached, stored) = FenceAck::channel();
                if events.send(TurnEvent::Fence(reached)).is_ok() {
                    let _ = tokio::time::timeout(FENCE_TIMEOUT, stored).await;
                }
            }
            None => {}
        }
    }
}

/// Events on their way to the publisher. An event is queued here before any
/// wait, so dropping a flush that is parked on a full queue (a cancel, the idle
/// timeout) leaves the event in place for the next flush instead of losing it.
struct EventOutbox {
    tx: mpsc::Sender<Queued>,
    pending: VecDeque<Event>,
    lost: usize,
}

impl EventOutbox {
    fn new(tx: mpsc::Sender<Queued>) -> Self {
        Self {
            tx,
            pending: VecDeque::new(),
            lost: 0,
        }
    }

    fn push(&mut self, event: Event) {
        self.pending.push_back(event);
    }

    /// Sends every pending event in order, waiting while the queue is full.
    /// Cancel safe: an event leaves `pending` only when its place is reserved.
    /// Returns how many events the closed queue refused.
    async fn flush(&mut self) -> usize {
        let mut refused = 0;
        while !self.pending.is_empty() {
            if let Ok(permit) = self.tx.reserve().await {
                if let Some(event) = self.pending.pop_front() {
                    permit.send(Queued::Event(event));
                }
            } else {
                refused += self.pending.len();
                self.pending.clear();
            }
        }
        self.lost += refused;
        refused
    }

    /// Events that never reached the publisher.
    fn lost(&self) -> usize {
        self.lost
    }
}

/// Where a running turn's adapter puts its events. A full queue makes the adapter
/// wait, so a slow store slows the backend instead of growing memory.
struct EventSink {
    outbox: EventOutbox,
    watchdog: Arc<Watchdog>,
    turns: Arc<Turns>,
    session: SessionId,
}

impl EventSink {
    /// Sends what an interrupted emit left behind, then reports the events lost.
    async fn finish(&mut self) -> usize {
        self.outbox.flush().await;
        self.outbox.lost()
    }
}

impl Emit for EventSink {
    async fn emit(&mut self, event: Event) {
        self.watchdog.observe(&event);
        if matches!(event, Event::Result { .. } | Event::TurnInterrupted { .. })
            && let Some(reservation) = self.turns.running().get_mut(&self.session)
        {
            reservation.wind_down();
        }
        self.outbox.push(event);
        let refused = self.outbox.flush().await;
        if refused > 0 {
            warn!(session = %self.session, refused, "the event publisher is gone; dropping events");
        }
    }
}

/// Sends what a cancel or the idle timeout cut off mid-send, closes the queue and
/// waits for the publisher. Returns how many events never reached the store.
async fn settle_events(
    session: &SessionId,
    mut emit: EventSink,
    publisher: &mut JoinHandle<usize>,
    limit: Option<Duration>,
) -> usize {
    let mut failures = 0;
    if let Some(lost) = within(limit, emit.finish()).await {
        failures += lost;
    } else {
        warn!(session = %session, "the store did not answer; the last events were not sent");
        failures += 1;
    }
    // Dropping the emitter closes the channel, which ends the publisher.
    drop(emit);
    match within(limit, &mut *publisher).await {
        Some(Ok(failed)) => failures += failed,
        Some(Err(_)) => {
            warn!(session = %session, "the event publisher stopped early");
            failures += 1;
        }
        None => {
            warn!(session = %session, "the store did not answer; dropping the queued events");
            publisher.abort();
            failures += 1;
        }
    }
    failures
}

/// Waits for `work`, or for `limit` when there is one. `None` means the limit passed.
async fn within<T>(limit: Option<Duration>, work: impl Future<Output = T>) -> Option<T> {
    match limit {
        Some(limit) => tokio::time::timeout(limit, work).await.ok(),
        None => Some(work.await),
    }
}

async fn publish_events(
    hub: Arc<Hub>,
    session: SessionId,
    turn: TurnId,
    mut events: mpsc::Receiver<Queued>,
) -> usize {
    let mut held = None;
    let mut failed = 0;
    loop {
        let next = match held.take() {
            Some(queued) => queued,
            None => match events.recv().await {
                Some(queued) => queued,
                None => return failed,
            },
        };
        let first = match next {
            Queued::Event(event) => event,
            Queued::Fence(reached) => {
                // The asker may have stopped waiting.
                let _ = reached.send(());
                continue;
            }
        };
        let event = coalesce_text(first, &mut events, &mut held).await;
        if let Err(e) = store_event(&hub, &session, &turn, &event).await {
            warn!(session = %session, error = %e, "could not store an event");
            failed += 1;
        }
    }
}

/// Joins the text deltas that follow `first` within [`DELTA_WINDOW`] into one.
/// The first event that is not part of the chunk is left in `held`, so order
/// is kept and no event is skipped.
async fn coalesce_text(
    first: Event,
    events: &mut mpsc::Receiver<Queued>,
    held: &mut Option<Queued>,
) -> Event {
    let Event::StreamDelta {
        session_id,
        turn_id,
        mut text,
    } = first
    else {
        return first;
    };
    let deadline = tokio::time::Instant::now() + DELTA_WINDOW;
    while text.len() < MAX_DELTA_CHUNK_BYTES {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(Queued::Event(Event::StreamDelta { text: more, .. }))) => {
                text.push_str(&more);
            }
            Ok(other) => {
                *held = other;
                break;
            }
            Err(_) => break,
        }
    }
    Event::StreamDelta {
        session_id,
        turn_id,
        text,
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

/// Removes the pidfile only while it still names `pid`: a turn of the same session may
/// have started a new server, and its pidfile is not ours to delete.
fn remove_pidfile_of(dir: &std::path::Path, session: &SessionId, pid: Option<u32>) {
    let names_pid =
        pidfile::read(&pidfile_path(dir, session)).is_ok_and(|record| Some(record.pid) == pid);
    if names_pid {
        remove_pidfile(dir, session);
    }
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

/// Answers `agent.run`.
pub(crate) async fn run_agent(turns: Option<&Arc<Turns>>, params: Option<Value>) -> HandlerResult {
    let turns = turns.ok_or_else(unavailable)?;
    let turn_id = turns
        .run_agent(parse_params(params)?)
        .await
        .map_err(TurnError::into_rpc)?;
    to_value(&AgentRunResult { turn_id })
}

/// Answers `session.cancel`.
pub(crate) async fn cancel(turns: Option<&Arc<Turns>>, params: Option<Value>) -> HandlerResult {
    let turns = turns.ok_or_else(unavailable)?;
    let params: SessionCancelParams = parse_params(params)?;
    let cancelled = if params.force {
        turns.force_cancel(&params.session_id).await
    } else {
        turns.cancel(&params.session_id)
    };
    to_value(&SessionCancelResult { cancelled })
}

/// Answers `session.close`.
pub(crate) async fn close(turns: Option<&Arc<Turns>>, params: Option<Value>) -> HandlerResult {
    let turns = turns.ok_or_else(unavailable)?;
    let params: SessionCloseParams = parse_params(params)?;
    let closed = turns
        .close(&params.session_id)
        .await
        .map_err(TurnError::into_rpc)?;
    to_value(&SessionCloseResult { closed })
}

/// Answers `session.rewind`.
pub(crate) async fn rewind(turns: Option<&Arc<Turns>>, params: Option<Value>) -> HandlerResult {
    let turns = turns.ok_or_else(unavailable)?;
    let params: SessionRewindParams = parse_params(params)?;
    let result = turns.rewind(params).await.map_err(TurnError::into_rpc)?;
    to_value(&result)
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
