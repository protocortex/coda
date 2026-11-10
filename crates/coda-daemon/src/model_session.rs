// SPDX-License-Identifier: Apache-2.0

//! Model-API sessions: a `coda_core` turn loop driven by a configured
//! provider, with its events stored through the hub and its permission
//! prompts answered through the daemon's permission flow.
//!
//! A session keeps its loop across sends; a send runs on its own task, with
//! the loop out of the table until the send ends.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use coda_agents::{Catalog, SkillTool};
use coda_config::config::{Config, Dirs, ProviderConfig};
use coda_core::broker::BrokerError;
use coda_core::history::{
    ASSISTANT_MESSAGE_KIND, Rebuild, SESSION_STARTED_KIND, TOOL_FINISHED_KIND,
    assistant_body_with_reasoning, empty_text_id, tool_finished_body,
};
pub use coda_core::history::{CONTEXT_OVERFLOW_KIND, MODEL_SWITCHED_KIND, TURN_STOPPED_KIND};
use coda_core::prompt::{Conversation, Prefix, PromptError, ToolSchema};
use coda_core::steer::SteerQueue;
use coda_core::tool_servers::{ToolServers, offers_tools, with_tool_servers};
use coda_core::tools::{
    BaseTools, BaseToolsConfig, StoreOriginals, ToolsError, ask_schema, base_tools, refresh_index,
    task_schema, todo_schema, with_fetch, with_recall,
};
use coda_core::turn::{CancelToken, Fallback, StopCondition, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::events::{
    CALL_RECOVERED_KIND, CallRecoveredBody, Event, GUARD_CONTINUE_KIND, GUARD_LIMIT_KIND,
    GuardContinueBody, GuardLimitBody, HOOK_BLOCKED_KIND, HookBlockedBody, MODEL_FALLBACK_KIND,
    ModelFallbackBody, TIMEOUT_KIND, TODO_UPDATED_KIND, TodoItem, TodoUpdatedBody, ToolCallStatus,
    tool_summary,
};
use coda_protocol::types::{
    BackendKind, ContextFill, OriginalId, PermissionDecision, SessionId, TokenUsage, TurnId,
};
use coda_providers::faux::{FauxError, FauxProvider};
use coda_providers::model::{ModelProvider, ModelRequest, ModelStream, ProviderError};
use coda_providers::registry::{
    ANTHROPIC_KEY, ModelClient, ModelRegistry, OPENAI_COMPAT_KEY, RegistryError, provider_key,
};
use coda_store::{EventRecord, NewQueued, SessionRecord, StoreError};
use coda_tools::bash::NAME as SHELL_TOOL;
use coda_tools::bash::{DEFAULT_ENV_ALLOWLIST, allowlisted_env};
use coda_tools::tool_server::Started;
use coda_tools::{Tool, fetch, skill};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tracing::{Instrument, Span, info, warn};

use crate::checkpoint;
use crate::gate::{BrokerGate, Brokers};
use crate::idle_pool::IdlePool;
use crate::pr_watches::{PrWatches, WatchPrTool};
use crate::ship::{
    ShipTools, creates_pull_request, ended_cleanly, origin_host, printed_pull_request_of,
};
use crate::subscribe::{Hub, HubError};
use crate::trace::backend_span;
use crate::turn::{
    DELTA_WINDOW, MAX_DELTA_CHUNK_BYTES, Reservation, STEERED_FIELD, TurnError, TurnState, Turns,
    USER_MESSAGE_KIND, store_event,
};

mod task;

const FAUX_KEY: &str = "faux";
/// Tool output above this many bytes is shortened for the model and kept whole.
const COMPACT_THRESHOLD_BYTES: usize = 16 * 1024;
const SYSTEM_PROMPT: &str = "You are Coda, a coding assistant working in the user's repository. \
Use the tools to read and change files and to run commands, and answer concisely.";
/// The gate's turn until the first send sets the real one.
const NO_TURN: &str = "";
/// Idle conversations kept in memory; the least recently used is dropped.
const MAX_IDLE_LOOPS: usize = 64;
/// How long a shutdown waits for cancelled sends to wind down.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const SHUTDOWN_POLL: Duration = Duration::from_millis(50);
/// How long a forced cancel waits for the store to record the end of the turn.
const FORCED_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a model-API session could not be started.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("provider {0} is disabled; set providers.{0}.enabled = true to use it")]
    Disabled(&'static str),
    #[error("backend {0:?} is not a model-API backend")]
    NotAModelBackend(BackendKind),
    #[error(transparent)]
    Faux(#[from] FauxError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Tools(#[from] ToolsError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
    #[error(transparent)]
    Broker(#[from] BrokerError),
    #[error(transparent)]
    Hub(#[from] HubError),
}

/// Why a session could not be switched to another model.
#[derive(Debug, thiserror::Error)]
pub enum SwitchError {
    #[error("session {0} has a turn running; switch models when it is idle")]
    Busy(SessionId),
    #[error("session {0} does not use a model-API backend")]
    NotAModelBackend(SessionId),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error(transparent)]
    Hub(#[from] HubError),
}

/// The provider a session was configured with.
enum Provider {
    /// Shared with the children the session starts, which draw from the same script.
    Faux(Arc<FauxProvider>),
    Api(ModelClient),
    /// Fails the task that drives it, to exercise panic handling.
    #[cfg(test)]
    Panicking,
}

impl ModelProvider for Provider {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProviderError> {
        match self {
            Self::Faux(provider) => provider.stream(request).await,
            Self::Api(provider) => provider.stream(request).await,
            #[cfg(test)]
            Self::Panicking => panic!("the test provider panics on purpose"),
        }
    }
}

/// Per-send overrides of the configured loop limits.
#[derive(Debug, Clone, Copy, Default)]
pub struct Limits {
    pub max_turns: Option<u32>,
    pub token_cap: Option<u64>,
}

/// What one snapshot of the definitions offers the model: the skills of the
/// `skill` tool and the agents the `task` tool lists. Both sit in the cached
/// prefix of a request, so a loop is rebuilt when either changes.
#[derive(Debug, PartialEq, Eq)]
struct Offer {
    skills: Option<Arc<SkillTool>>,
    /// Each agent's name and description.
    agents: Vec<(String, String)>,
}

impl Offer {
    fn of(catalog: &Catalog) -> Self {
        Self {
            skills: SkillTool::from_catalog(catalog).map(Arc::new),
            agents: catalog
                .agents()
                .map(|entry| (entry.def.name.clone(), entry.def.description.clone()))
                .collect(),
        }
    }
}

/// One session's loop with the channel its events arrive on.
struct ModelLoop {
    turn_loop: TurnLoop<Provider, Arc<BrokerGate>>,
    events: UnboundedReceiver<TurnEvent>,
    turn: Arc<Mutex<TurnId>>,
    limits: Limits,
    /// What this loop offers the model, to tell at the next send whether the
    /// files or the trust decision changed.
    offer: Offer,
    /// How many definition files had problems when this loop last said so, so
    /// the daemon log repeats the warning only when that number changes.
    problems: usize,
}

/// What a loop takes over from the idle one it replaces: the send limits and
/// the tokens already spent, so a rebuild never hands the session a fresh budget.
#[derive(Debug, Clone, Copy)]
struct Carried {
    limits: Limits,
    used: u64,
}

/// Most dropped conversations whose [`Carried`] state is kept: as many as the
/// idle pool holds.
const MAX_CARRIED: usize = MAX_IDLE_LOOPS;

/// The [`Carried`] state of conversations that were dropped, oldest first, until
/// the next send of their session builds the replacement. A session that is
/// never used again would leave its entry for good, so the oldest entry is
/// forgotten at [`MAX_CARRIED`]; that session then restarts from the limits of
/// its next send and its stored usage.
#[derive(Debug, Default)]
struct Dropped(VecDeque<(SessionId, Carried)>);

impl Dropped {
    fn insert(&mut self, session: SessionId, carried: Carried) {
        self.remove(&session);
        self.0.push_back((session, carried));
        if self.0.len() > MAX_CARRIED {
            self.0.pop_front();
        }
    }

    fn get(&self, session: &SessionId) -> Option<Carried> {
        let found = self.0.iter().find(|(id, _)| id == session);
        found.map(|(_, carried)| *carried)
    }

    fn remove(&mut self, session: &SessionId) {
        self.0.retain(|(id, _)| id != session);
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

impl ModelLoop {
    fn carried(&self) -> Carried {
        Carried {
            limits: self.limits,
            used: self.turn_loop.used_tokens(),
        }
    }
}

/// Removes a session from the running table when dropped, so a task that
/// panics never leaves the session busy. A panic also ends the turn; the loop
/// went down with the task, so the next send rebuilds it.
struct Slot {
    sessions: Arc<ModelSessions>,
    session: SessionId,
    gate: Arc<ForceGate>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GateState {
    Open,
    Forced,
    Shut,
}

/// Lets a forced cancel take a turn task away from its send, until the send
/// has its outcome and is on its way to record the end of the turn itself.
struct ForceGate {
    state: Mutex<GateState>,
    wake: Notify,
}

impl ForceGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState::Open),
            wake: Notify::new(),
        })
    }

    /// Asks the task to give up on its turn. Nothing happens once the send has shut the gate.
    fn force(&self) {
        let mut state = lock(&self.state);
        if *state == GateState::Open {
            *state = GateState::Forced;
            self.wake.notify_one();
        }
    }

    /// Called by the send when it has its outcome. `false` when a force came
    /// first, so the task is about to be dropped and the send must not record anything.
    fn shut(&self) -> bool {
        let mut state = lock(&self.state);
        if *state == GateState::Open {
            *state = GateState::Shut;
        }
        *state == GateState::Shut
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        lock(&self.sessions.forces).remove(&self.session);
        lock(&self.sessions.steers).remove(&self.session);
        let armed = self
            .sessions
            .running()
            .remove(&self.session)
            .and_then(Reservation::into_armed);
        if let (true, Some((_, turn))) = (std::thread::panicking(), armed) {
            warn!(session = %self.session, turn = %turn, "the turn panicked");
            let (hub, session) = (Arc::clone(&self.sessions.hub), self.session.clone());
            tokio::spawn(async move { close_turn(&hub, &session, &turn, true).await });
        }
    }
}

pub type EnvReader = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The model-API sessions of the daemon.
pub struct ModelSessions {
    hub: Arc<Hub>,
    brokers: Arc<Brokers>,
    config: Config,
    registry: ModelRegistry,
    env: Vec<(OsString, OsString)>,
    /// The tool servers the user listed, each with the variables it named.
    tool_servers: ToolServers,
    key_reader: EnvReader,
    /// Present from the moment a send reserves the session, with its cancel
    /// token and turn once it runs; never held across an await.
    running: Mutex<HashMap<SessionId, Reservation<(CancelToken, TurnId)>>>,
    /// The gate of each reserved session, for a cancel that did not take.
    forces: Mutex<HashMap<SessionId, Arc<ForceGate>>>,
    /// Idle conversations, least recently used first.
    loops: IdlePool<ModelLoop>,
    /// Limits and spent tokens of conversations a rebuild dropped, until the
    /// next send builds the replacement.
    carried: Mutex<Dropped>,
    /// What the runners of the sessions' `task` calls share.
    children: Arc<task::ChildEnv>,
    /// Called with a session whose turn ended cleanly, once the session is free.
    on_clean_end: OnceLock<CleanEnd>,
    /// The pull request watches the `watch_pr` tool starts; the tool is offered once set.
    pr_watches: OnceLock<Arc<PrWatches>>,
    /// Where a message steered into each running send goes.
    steers: Mutex<HashMap<SessionId, SteerQueue>>,
}

/// What runs after a turn ends cleanly and frees its session.
pub(crate) type CleanEnd = Arc<dyn Fn(SessionId) + Send + Sync>;

impl ModelSessions {
    /// Sessions that read API keys through `key_reader` and keep only the
    /// allowlisted part of `env`.
    #[must_use]
    pub fn new(
        hub: Arc<Hub>,
        brokers: Arc<Brokers>,
        config: Config,
        env: Vec<(OsString, OsString)>,
        key_reader: EnvReader,
    ) -> Arc<Self> {
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        let home = match Dirs::from_lookup(lookup) {
            Ok(dirs) => Some(dirs.base().to_path_buf()),
            Err(error) => {
                warn!(%error, "the user's definitions directory cannot be found; sub-agents will not see the user's agents");
                None
            }
        };
        let registry = ModelRegistry::from_config(&config);
        let tool_servers = ToolServers::new(&config.tool_servers, &env);
        let env = allowlisted_env(DEFAULT_ENV_ALLOWLIST, env);
        let children = Arc::new(task::ChildEnv {
            hub: Arc::clone(&hub),
            config: config.clone(),
            registry: registry.clone(),
            env: env.clone(),
            key_reader: Arc::clone(&key_reader),
            home,
        });
        Arc::new(Self {
            hub,
            brokers,
            registry,
            config,
            env,
            tool_servers,
            key_reader,
            running: Mutex::default(),
            forces: Mutex::default(),
            loops: IdlePool::new(MAX_IDLE_LOOPS),
            carried: Mutex::default(),
            children,
            on_clean_end: OnceLock::new(),
            pr_watches: OnceLock::new(),
            steers: Mutex::default(),
        })
    }

    /// Offers the `watch_pr` tool to sessions built from now on. Only the first call counts.
    pub(crate) fn set_pr_watches(&self, watches: Arc<PrWatches>) {
        let _ = self.pr_watches.set(watches);
    }

    /// Sets what runs after a turn ends cleanly. Only the first call counts.
    pub(crate) fn set_on_clean_end(&self, after: CleanEnd) {
        let _ = self.on_clean_end.set(after);
    }

    /// The agent definitions a session in `cwd` may run.
    pub(crate) async fn catalog(&self, cwd: &str) -> Catalog {
        self.children.catalog(cwd).await
    }

    /// The enabled backends a session can start on: agent backends first,
    /// then the model APIs.
    pub(crate) fn available_backends(&self) -> Vec<BackendKind> {
        let mut backends = crate::backends::enabled_agent_backends(&self.config);
        backends.extend(self.registry.enabled_backends());
        backends
    }

    /// Starts a send with `text` and returns without waiting for it to end.
    /// `limits` apply only when this send has to build the session's loop; a
    /// loop kept from an earlier send keeps its own, and a model switch hands
    /// the dropped loop's limits and spent tokens to the loop that replaces it.
    ///
    /// # Errors
    /// Returns [`TurnError::Busy`] when the session is running,
    /// [`TurnError::Model`] when its provider or tools cannot be set up, and
    /// [`TurnError::Hub`] when the store refuses.
    pub(crate) async fn send(
        self: &Arc<Self>,
        record: SessionRecord,
        text: String,
        limits: Limits,
        request: Option<String>,
    ) -> Result<TurnId, TurnError> {
        let session = record.id.clone();
        let slot = self.reserve(&session)?;
        // The caller's record may predate a model switch that finished while
        // this send was starting; under the reservation the stored one is final.
        let id = session.clone();
        let record = self.hub.with_store(move |s| s.load_session(&id)).await?;
        info!(session = %session, step = "build_loop", "preparing the model session");
        let model_loop = self.loop_for_send(&record, limits, &text).await?;
        info!(session = %session, step = "begin_turn", "recording the user message");
        let turn = match Turns::begin_for(&self.hub, &session, &text, request.as_deref()).await {
            Ok(turn) => turn,
            Err(e) => {
                self.keep_loop(session, model_loop);
                return Err(e.into());
            }
        };
        *lock(&model_loop.turn) = turn.clone();
        // Reset before arming: a cancel from here on must stop this send. Armed
        // before any further await, so a panic from here on still ends the turn.
        let cancel = model_loop.turn_loop.cancel_token();
        cancel.reset();
        slot.arm((cancel, turn.clone()));
        lock(&self.steers).insert(session.clone(), model_loop.turn_loop.steer_queue());
        self.keep_prompt(&session, &text).await;
        let span = backend_span(&session, &turn, backend_name(record.backend));
        let sessions = Arc::clone(self);
        let task_turn = turn.clone();
        let gate = Arc::clone(&slot.gate);
        self.drive(
            slot,
            turn.clone(),
            span,
            sessions.run(model_loop, session, task_turn, text, gate),
        );
        Ok(turn)
    }

    /// Runs `work`, the send of `turn`, on its own task. A forced cancel drops
    /// `work` and ends the turn here, so a send stuck where a cancel cannot
    /// reach it still lets the session go.
    fn drive(
        self: &Arc<Self>,
        slot: Slot,
        turn: TurnId,
        span: Span,
        work: impl Future<Output = bool> + Send + 'static,
    ) {
        let gate = Arc::clone(&slot.gate);
        let sessions = Arc::clone(self);
        tokio::spawn(
            async move {
                let clean = tokio::select! {
                    biased;
                    () = gate.wake.notified() => {
                        warn!(session = %slot.session, turn = %turn, "the turn did not stop; giving up on it");
                        sessions.end_forced(&slot.session, &turn).await;
                        false
                    }
                    clean = work => clean,
                };
                let session = slot.session.clone();
                drop(slot);
                if let (true, Some(after)) = (clean, sessions.on_clean_end.get()) {
                    after(session);
                }
            }
            .instrument(span),
        );
    }

    /// Records the end of a turn that was given up on, as cancelled. A store
    /// that does not answer cannot keep the session busy for good.
    async fn end_forced(&self, session: &SessionId, turn: &TurnId) {
        let body = json!({ "condition": StopCondition::Cancelled.as_str() });
        let record = async {
            log_failure(publish_kind(&self.hub, session, turn, TURN_STOPPED_KIND, &body).await);
            close_turn(&self.hub, session, turn, false).await;
        };
        if tokio::time::timeout(FORCED_CLOSE_TIMEOUT, record)
            .await
            .is_err()
        {
            warn!(session = %session, turn = %turn, "could not record the end of the turn in time");
        }
    }

    /// Switches an idle model-API session to the alias `alias`.
    ///
    /// The steps: reserve the session so no send can start, resolve the alias
    /// and build its client to prove it is usable, store the resolved model id,
    /// drop the idle loop and record the switch. The next send builds a new loop
    /// with a fresh prefix and the history rebuilt from the stored events, and
    /// keeps the send limits and spent tokens of the dropped loop. The alias name lives only in
    /// the switch event, so a later change to the aliases cannot reinterpret the
    /// stored model. Only `session` is touched. It refuses a cancelled send that
    /// has not ended; the `session.set_model` handler waits for that first.
    ///
    /// # Errors
    /// Returns [`SwitchError::Busy`] while a send runs,
    /// [`SwitchError::NotAModelBackend`] for other backends,
    /// [`SwitchError::Registry`] when the alias is unknown or its client cannot
    /// be built, and [`SwitchError::Hub`] when the store refuses.
    pub(crate) async fn set_model(
        self: &Arc<Self>,
        session: &SessionId,
        alias: &str,
    ) -> Result<SessionRecord, SwitchError> {
        let id = session.clone();
        let record = self.hub.with_store(move |s| s.load_session(&id)).await?;
        if !matches!(
            record.backend,
            BackendKind::Anthropic | BackendKind::OpenaiCompat | BackendKind::Faux
        ) {
            return Err(SwitchError::NotAModelBackend(session.clone()));
        }
        // A switch has no turn, so a cancel finds nothing to stop.
        let _slot = self
            .reserve_as(session, Reservation::uncancellable())
            .map_err(|_| SwitchError::Busy(session.clone()))?;
        info!(session = %session, step = "resolve_alias", "checking the model alias");
        let target = self.registry.alias(alias, record.backend)?;
        if record.backend != BackendKind::Faux {
            let reader = Arc::clone(&self.key_reader);
            self.registry.build(&target, move |name| reader(name))?;
        }
        info!(session = %session, step = "store_switch", "storing the new model");
        let (id, model) = (session.clone(), target.model.clone());
        let turn = self
            .hub
            .with_store_mut(move |store| {
                store.set_session_model(&id, &model)?;
                // The switch is an event with no work behind it, so its turn
                // is over at once instead of looking cut short after a restart.
                let turn = store.begin_turn(&id)?;
                store.end_turn(&id, &turn)?;
                Ok(turn)
            })
            .await?;
        if let Some(dropped) = self.take_loop(session) {
            lock(&self.carried).insert(session.clone(), dropped.carried());
        }
        let body = json!({
            "alias": alias,
            "provider": backend_name(record.backend),
            "model": target.model,
        });
        log_failure(publish_kind(&self.hub, session, &turn, MODEL_SWITCHED_KIND, &body).await);
        let id = session.clone();
        Ok(self.hub.with_store(move |s| s.load_session(&id)).await?)
    }

    /// Adds `text` to the running send of `session` and returns its turn. `None`
    /// when no send is running, or it is already ending.
    pub(crate) fn steer(&self, session: &SessionId, text: &str) -> Option<TurnId> {
        let turn = {
            let running = self.running();
            let reservation = running.get(session)?;
            if reservation.state() != TurnState::Live {
                return None;
            }
            reservation.armed().map(|(_, turn)| turn.clone())?
        };
        lock(&self.steers).get(session)?.push(text.to_owned());
        Some(turn)
    }

    /// Cancels the running send of `session`; `false` when none is running.
    /// A send that is still starting is cancelled as soon as it is armed.
    pub fn cancel(&self, session: &SessionId) -> bool {
        match self.running().get_mut(session) {
            Some(reservation) => reservation.request_cancel(|(cancel, _)| cancel.cancel()),
            None => false,
        }
    }

    /// Cancels like [`Self::cancel`], then gives up on the turn task at once
    /// instead of waiting for it to notice. For a stop that did not take.
    pub fn force_cancel(&self, session: &SessionId) -> bool {
        let cancelled = self.cancel(session);
        if cancelled && let Some(gate) = lock(&self.forces).get(session) {
            gate.force();
        }
        cancelled
    }

    /// Drops the idle conversation of `session`; `false` when it held none.
    /// Its stored events stay, so a later send rebuilds the loop from them.
    pub(crate) fn close(&self, session: &SessionId) -> bool {
        lock(&self.carried).remove(session);
        self.take_loop(session).is_some()
    }

    /// Drops the idle loop of `session` but hands its spent budget to the next one.
    pub(crate) fn drop_idle_loop(&self, session: &SessionId) {
        if let Some(dropped) = self.take_loop(session) {
            lock(&self.carried).insert(session.clone(), dropped.carried());
        }
    }

    /// The state of the send of `session`, or `None` when it has none.
    pub(crate) fn turn_state(&self, session: &SessionId) -> Option<TurnState> {
        self.running().get(session).map(Reservation::state)
    }

    /// Whether `session` has a send starting or running.
    #[must_use]
    pub fn is_running(&self, session: &SessionId) -> bool {
        self.running().contains_key(session)
    }

    /// Cancels every running send and closes the turns of any that outlast the grace period.
    pub async fn shutdown(&self) {
        self.shutdown_within(SHUTDOWN_GRACE).await;
    }

    async fn shutdown_within(&self, grace: Duration) {
        for reservation in self.running().values_mut() {
            reservation.request_cancel(|(cancel, _)| cancel.cancel());
        }
        let idle = async {
            while self.active() > 0 {
                tokio::time::sleep(SHUTDOWN_POLL).await;
            }
        };
        if tokio::time::timeout(grace, idle).await.is_err() {
            let stuck: Vec<_> = self
                .running()
                .iter()
                .filter_map(|(session, r)| Some((session.clone(), r.armed()?.1.clone())))
                .collect();
            for (session, turn) in stuck {
                warn!(session = %session, turn = %turn, "the turn did not stop in time");
                close_turn(&self.hub, &session, &turn, true).await;
            }
        }
    }

    /// How many sessions have a send starting or running.
    #[must_use]
    pub fn active(&self) -> usize {
        self.running().len()
    }

    /// Keeps a send from starting on `session` until the returned guard drops.
    pub(crate) fn hold(
        self: &Arc<Self>,
        session: &SessionId,
    ) -> Result<Box<dyn std::any::Any + Send>, TurnError> {
        Ok(Box::new(self.reserve(session)?))
    }

    /// The sessions that have a send starting or running.
    pub(crate) fn running_sessions(&self) -> Vec<SessionId> {
        self.running().keys().cloned().collect()
    }

    fn running(&self) -> MutexGuard<'_, HashMap<SessionId, Reservation<(CancelToken, TurnId)>>> {
        lock(&self.running)
    }

    fn take_loop(&self, session: &SessionId) -> Option<ModelLoop> {
        self.loops.take(session)
    }

    fn keep_loop(&self, session: SessionId, model_loop: ModelLoop) {
        // Held across the eviction, so a send for the evicted session that finds
        // it gone from the pool finds its state in the table too.
        let mut carried = lock(&self.carried);
        if let Some((evicted, dropped)) = self.loops.keep(session, model_loop) {
            info!(session = %evicted, "dropped the least recently used idle conversation");
            // The next send rebuilds it, and must not hand it a fresh budget.
            carried.insert(evicted, dropped.carried());
        }
    }

    fn reserve(self: &Arc<Self>, session: &SessionId) -> Result<Slot, TurnError> {
        self.reserve_as(session, Reservation::default())
    }

    fn reserve_as(
        self: &Arc<Self>,
        session: &SessionId,
        reservation: Reservation<(CancelToken, TurnId)>,
    ) -> Result<Slot, TurnError> {
        match self.running().entry(session.clone()) {
            std::collections::hash_map::Entry::Occupied(_) => {
                return Err(TurnError::Busy(session.clone()));
            }
            std::collections::hash_map::Entry::Vacant(vacant) => vacant.insert(reservation),
        };
        let gate = ForceGate::new();
        lock(&self.forces).insert(session.clone(), Arc::clone(&gate));
        Ok(Slot {
            sessions: Arc::clone(self),
            session: session.clone(),
            gate,
        })
    }

    /// Keeps the prompt whole in `originals`; a failure only costs that copy.
    async fn keep_prompt(&self, session: &SessionId, text: &str) {
        let (id, prompt) = (session.clone(), text.to_owned());
        let stored = self
            .hub
            .with_store_mut(move |store| store.store_original(&id, &prompt))
            .await;
        if let Err(e) = stored {
            warn!(session = %session, error = %e, "could not keep the prompt");
        }
    }

    fn section(&self, key: &'static str) -> Result<&ProviderConfig, ModelError> {
        self.config
            .providers
            .get(key)
            .filter(|section| section.enabled)
            .ok_or(ModelError::Disabled(key))
    }

    fn provider(&self, record: &SessionRecord) -> Result<Provider, ModelError> {
        match record.backend {
            BackendKind::Faux => {
                self.section(FAUX_KEY)?;
                #[cfg(test)]
                if record.model.as_deref() == Some("panic") {
                    return Ok(Provider::Panicking);
                }
                Ok(Provider::Faux(Arc::new(FauxProvider::from_config(
                    &self.config,
                )?)))
            }
            BackendKind::Anthropic | BackendKind::OpenaiCompat => {
                let target = self
                    .registry
                    .select(record.backend, record.model.as_deref())?;
                let reader = Arc::clone(&self.key_reader);
                let lookup = move |name: &str| reader(name);
                Ok(Provider::Api(self.registry.build(&target, lookup)?))
            }
            other => Err(ModelError::NotAModelBackend(other)),
        }
    }

    /// The idle loop of the session, or a new one that continues from the
    /// history the session stored. The definitions are read once for the send.
    /// An idle loop whose offer (skills and agents) no longer matches them is
    /// dropped, so a revoked or edited project skill or agent is gone by this
    /// send; the limits and spent tokens carry over. When the trust decision
    /// cannot be read, an idle loop is kept as it is for this send, since the
    /// failure says nothing about the files, and only a session with no idle
    /// loop is built on the built-in agents.
    async fn loop_for_send(
        &self,
        record: &SessionRecord,
        limits: Limits,
        text: &str,
    ) -> Result<ModelLoop, ModelError> {
        let catalog = match self.children.read_catalog(&record.cwd).await {
            Ok(catalog) => catalog,
            Err(error) => {
                if let Some(model_loop) = self.take_loop(&record.id) {
                    warn!(session = %record.id, %error, "the definitions could not be read; keeping the conversation as it is");
                    return Ok(model_loop);
                }
                warn!(session = %record.id, %error, "the definitions could not be read; only the built-in agents are offered");
                Catalog::builtin()
            }
        };
        let offer = Offer::of(&catalog);
        let problems = catalog.problems().len();
        if let Some(mut model_loop) = self.take_loop(&record.id) {
            if model_loop.offer == offer {
                if model_loop.problems != problems {
                    warn_skipped(&record.id, problems);
                    model_loop.problems = problems;
                }
                return Ok(model_loop);
            }
            info!(session = %record.id, "the skills or agents changed; rebuilding the conversation");
            lock(&self.carried).insert(record.id.clone(), model_loop.carried());
        }
        let kept = lock(&self.carried).get(&record.id);
        let carried = match kept {
            Some(carried) => carried,
            None => Carried {
                limits,
                used: self.stored_tokens(&record.id).await?,
            },
        };
        warn_skipped(&record.id, problems);
        let built = self.build(record, carried, offer, problems, text).await?;
        lock(&self.carried).remove(&record.id);
        Ok(built)
    }

    /// The tokens the session's stored usage events add up to, which is what a
    /// loop built after a restart or an eviction has already spent.
    async fn stored_tokens(&self, session: &SessionId) -> Result<u64, HubError> {
        let id = session.clone();
        let usage = self
            .hub
            .with_store(move |store| store.usage_summary(Some(&id)))
            .await?;
        Ok(usage.input_tokens.saturating_add(usage.output_tokens))
    }

    /// The context window of the model the session runs, when it is known.
    fn context_window(&self, record: &SessionRecord) -> Option<u32> {
        let target = self
            .registry
            .select(record.backend, record.model.as_deref())
            .ok()?;
        self.registry.context_window(&target)
    }

    /// Rebuilds the conversation one page of events at a time, so the store is
    /// free for other users between pages. Also says whether a `session_start`
    /// hook already passed for the session.
    async fn stored_conversation(
        &self,
        session: &SessionId,
        prefix: Prefix,
    ) -> Result<(Conversation, bool, Vec<TodoItem>), HubError> {
        let mut rebuild = Rebuild::new(prefix);
        let mut after = 0;
        loop {
            let id = session.clone();
            let (back, next) = self
                .hub
                .with_store(move |store| {
                    let next = rebuild.apply_page(store, &id, after)?;
                    Ok((rebuild, next))
                })
                .await?;
            rebuild = back;
            let Some(last) = next else {
                let started = rebuild.session_started();
                let todos = rebuild.todos().to_vec();
                return Ok((rebuild.finish(), started, todos));
            };
            after = last;
        }
    }

    /// The base tools of a session, `fetch` when the user listed a host, and
    /// `recall` when the user switched it on.
    fn session_tools(&self, record: &SessionRecord) -> Result<BaseTools, ToolsError> {
        let tools = base_tools(
            &BaseToolsConfig {
                workdir: PathBuf::from(&record.cwd),
                bash_timeout: self.config.timing.tool_execution,
                env: self.env.clone(),
            },
            self.hub.store_handle(),
            StoreOriginals::new(self.hub.store_handle(), record.id.clone()),
        )?;
        let tools = with_fetch(tools, &self.config.fetch);
        Ok(with_recall(
            tools,
            &self.config.recall,
            self.hub.store_handle(),
            record.id.clone(),
        ))
    }

    /// The map of the repository for a loop built now, or `None` when there is
    /// none. It is built once per loop, so it can differ between sessions and
    /// stays the same inside one.
    async fn repo_map(&self, record: &SessionRecord, text: &str) -> Option<String> {
        let (store, root, text) = (
            self.hub.store_handle(),
            PathBuf::from(&record.cwd),
            text.to_owned(),
        );
        let (window, config, env) = (
            self.context_window(record),
            self.config.repo_map.clone(),
            self.env.clone(),
        );
        let built = tokio::task::spawn_blocking(move || {
            coda_core::repo_map::build(&store, &root, &text, window, &config, env)
        })
        .await;
        built.unwrap_or_else(|error| {
            warn!(%error, "building the repository map crashed");
            None
        })
    }

    /// Starts the user's tool servers for a loop built now. The loop owns them
    /// through its tools, so they stop when it is dropped.
    async fn start_tool_servers(&self, record: &SessionRecord, tools: &BaseTools) -> Vec<Started> {
        if self.tool_servers.is_empty() {
            return Vec::new();
        }
        let mut reserved: Vec<String> = tools.schemas.iter().map(|s| s.name.clone()).collect();
        reserved.extend([task_schema(&[]).name, todo_schema().name, ask_schema().name]);
        let (servers, cwd) = (self.tool_servers.clone(), PathBuf::from(&record.cwd));
        let started = tokio::task::spawn_blocking(move || {
            let names: Vec<&str> = reserved.iter().map(String::as_str).collect();
            servers.start(&cwd, &names)
        })
        .await;
        started.unwrap_or_else(|error| {
            warn!(%error, "starting the tool servers crashed");
            Vec::new()
        })
    }

    /// The system prompt, with the rule about untrusted text added when the
    /// session can fetch pages and again when it offers tool server tools.
    fn system_prompt(&self, external: bool) -> String {
        let mut prompt = SYSTEM_PROMPT.to_owned();
        if !self.config.fetch.allow_hosts.is_empty() {
            prompt = format!("{prompt} {}", fetch::UNTRUSTED_RULE);
        }
        if external {
            prompt = format!("{prompt} {}", coda_tools::tool_server::UNTRUSTED_RULE);
        }
        prompt
    }

    async fn build(
        &self,
        record: &SessionRecord,
        Carried { limits, used }: Carried,
        offer: Offer,
        problems: usize,
        text: &str,
    ) -> Result<ModelLoop, ModelError> {
        let provider = self.provider(record)?;
        let faux = if let Provider::Faux(faux) = &provider {
            Some(Arc::clone(faux))
        } else {
            None
        };
        let shared_script = faux.clone();
        let (store, workdir) = (self.hub.store_handle(), PathBuf::from(&record.cwd));
        if let Err(error) =
            tokio::task::spawn_blocking(move || refresh_index(&store, &workdir)).await
        {
            warn!(%error, "the code index refresh crashed");
        }
        let mut tools = self.session_tools(record)?;
        if let Some(skills) = &offer.skills {
            tools.schemas.push(skill_schema(skills));
            tools.tools = tools.tools.with(Arc::clone(skills) as Arc<dyn Tool>);
        }
        if let Some(watches) = self.pr_watches.get() {
            tools.schemas.push(WatchPrTool::schema());
            let tool = WatchPrTool::new(
                Arc::clone(watches),
                record.id.clone(),
                PathBuf::from(&record.cwd),
            );
            tools.tools = tools.tools.with(Arc::new(tool) as Arc<dyn Tool>);
        }
        let offered_servers = self.start_tool_servers(record, &tools).await;
        let external = offers_tools(&offered_servers);
        let tools = with_tool_servers(tools, &offered_servers);
        let mut schemas = tools.schemas;
        schemas.push(task_schema(&offer.agents));
        schemas.push(todo_schema());
        schemas.push(ask_schema());
        let prefix = Prefix::new(&self.system_prompt(external), schemas, "")?;
        let (conversation, session_started, todos) =
            self.stored_conversation(&record.id, prefix).await?;
        let conversation = conversation.with_context(self.repo_map(record, text).await);
        let turn = Arc::new(Mutex::new(TurnId::new(NO_TURN)));
        let broker = self
            .brokers
            .broker(record, self.config.timing.permission_default_deny)
            .await?;
        let gate = Arc::new(BrokerGate::new(
            &self.hub,
            broker,
            record,
            Arc::clone(&turn),
        ));
        let (sender, events) = unbounded_channel();
        gate.fence().attach_model(sender.downgrade());
        let factory = task::LoopFactory::new(
            Arc::clone(&self.children),
            record.clone(),
            Arc::clone(&gate),
            faux,
            sender.clone(),
        );
        let mut turn_loop = factory
            .build_loop(
                0,
                task::LoopParts {
                    provider,
                    window: self.context_window(record),
                    gate,
                    tools: tools.tools,
                    conversation,
                    config: TurnConfig {
                        session: record.id.clone(),
                        cwd: PathBuf::from(&record.cwd),
                        max_turns: limits.max_turns.unwrap_or(self.config.max_turns),
                        token_limit: limits.token_cap.unwrap_or(self.config.token_cap),
                        compact_threshold_bytes: COMPACT_THRESHOLD_BYTES,
                    },
                    events: sender,
                    cancel: None,
                },
            )
            .with_used_tokens(used)
            .with_resumed(session_started)
            .with_text_tool_calls(self.text_tool_calls(record))
            .with_todos(todos)
            .with_ask();
        if let Some((alias, chain)) = self.fallbacks(record, shared_script.as_ref()) {
            turn_loop = turn_loop.with_fallbacks(alias, chain);
        }
        Ok(ModelLoop {
            turn_loop,
            events,
            turn,
            limits,
            offer,
            problems,
        })
    }

    /// The alias of the model the session runs and the models it falls back to,
    /// when an alias gives that model a chain. A fallback that cannot be built,
    /// because its provider is off or has no key, is left out and logged, so a
    /// broken chain never stops a session that runs on its first model. A
    /// scripted fallback draws turns from the same script as the session.
    fn fallbacks(
        &self,
        record: &SessionRecord,
        script: Option<&Arc<FauxProvider>>,
    ) -> Option<(String, Vec<Fallback<Provider>>)> {
        let target = self
            .registry
            .select(record.backend, record.model.as_deref())
            .ok()?;
        let chain = match self.registry.fallback_chain(&target) {
            Ok(chain) => chain?,
            Err(error) => {
                warn!(session = %record.id, %error, "the fallback chain cannot be used");
                return None;
            }
        };
        let mut fallbacks = Vec::new();
        for (alias, next) in chain.targets {
            let provider = if next.backend == BackendKind::Faux {
                script.map(|script| Provider::Faux(Arc::clone(script)))
            } else {
                let reader = Arc::clone(&self.key_reader);
                match self.registry.build(&next, move |name| reader(name)) {
                    Ok(client) => Some(Provider::Api(client)),
                    Err(error) => {
                        warn!(session = %record.id, %alias, %error, "a fallback model cannot be used and is left out");
                        None
                    }
                }
            };
            if let Some(provider) = provider {
                fallbacks.push(Fallback {
                    alias,
                    provider,
                    context_window: self.registry.context_window(&next),
                });
            }
        }
        Some((chain.alias, fallbacks))
    }

    /// Whether the provider that serves `record` is set to read a call from a
    /// reply's text.
    fn text_tool_calls(&self, record: &SessionRecord) -> bool {
        provider_key(record.backend)
            .and_then(|key| self.config.providers.get(key))
            .is_some_and(|section| section.text_tool_calls)
    }

    /// Queues the steered messages the loop ended before reading, so each starts
    /// as a turn of its own instead of being lost.
    async fn requeue_unread_steers(&self, session: &SessionId, model_loop: &ModelLoop) {
        lock(&self.steers).remove(session);
        for text in model_loop.turn_loop.steer_queue().take() {
            let (id, queued) = (session.clone(), text);
            let stored = self
                .hub
                .with_store_mut(move |store| {
                    store.enqueue_message(
                        &id,
                        NewQueued {
                            text: &queued,
                            max_turns: None,
                            token_cap: None,
                        },
                    )
                })
                .await;
            if let Err(error) = stored {
                warn!(session = %session, %error, "could not queue a steered message the turn did not read");
            }
        }
    }

    /// The named steps of a running send: drive the loop while storing its
    /// events, expire orphaned prompts, record the end.
    async fn run(
        self: Arc<Self>,
        mut model_loop: ModelLoop,
        session: SessionId,
        turn: TurnId,
        text: String,
        gate: Arc<ForceGate>,
    ) -> bool {
        let ModelLoop {
            turn_loop, events, ..
        } = &mut model_loop;
        info!(session = %session, turn = %turn, step = "drive", "driving the turn loop");
        let mut streamed = StreamedText::default();
        let outcome = {
            let send = turn_loop.send(&text);
            tokio::pin!(send);
            loop {
                tokio::select! {
                    outcome = &mut send => break outcome,
                    Some(event) = events.recv() => {
                        streamed.publish(&self.hub, &session, &turn, event).await;
                    }
                    () = streamed.due() => {
                        streamed.flush(&self.hub, &session, &turn).await;
                    }
                }
            }
        };
        if !gate.shut() {
            // The forced end is recorded by the task that holds this future.
            std::future::pending::<()>().await;
        }
        let clean = matches!(outcome, Ok(StopCondition::EndTurn));
        while let Ok(event) = events.try_recv() {
            streamed.publish(&self.hub, &session, &turn, event).await;
        }
        streamed.flush(&self.hub, &session, &turn).await;
        let failed = outcome.is_err();
        match &outcome {
            Ok(condition) => {
                let body = json!({ "condition": condition.as_str() });
                log_failure(
                    publish_kind(&self.hub, &session, &turn, TURN_STOPPED_KIND, &body).await,
                );
            }
            Err(e) => warn!(session = %session, turn = %turn, error = %e, "the turn failed"),
        }
        if let Some(reservation) = self.running().get_mut(&session) {
            reservation.wind_down();
        }
        self.requeue_unread_steers(&session, &model_loop).await;
        close_turn(&self.hub, &session, &turn, failed).await;
        self.keep_loop(session, model_loop);
        clean
    }
}

impl Slot {
    fn arm(&self, armed: (CancelToken, TurnId)) {
        if let Some(reservation) = self.sessions.running().get_mut(&self.session) {
            reservation.arm(armed, |(cancel, _)| cancel.cancel());
        }
    }
}

/// Ends a turn: expires its open prompts, records its result, finishes it.
async fn close_turn(hub: &Arc<Hub>, session: &SessionId, turn: &TurnId, failed: bool) {
    info!(session = %session, turn = %turn, step = "expire_prompts", "expiring open prompts");
    expire_open_prompts(hub, session, turn).await;
    let result = Event::Result {
        session_id: session.clone(),
        turn_id: turn.clone(),
        is_error: failed,
    };
    log_failure(store_event(hub, session, turn, &result).await);
    Turns::finish(hub, session, turn, failed).await;
    checkpoint::after_turn(hub, session, turn).await;
}

/// The schema of the skill tool: the index is its description, and only the
/// skills listed in it can be named.
fn skill_schema(skills: &SkillTool) -> ToolSchema {
    ToolSchema {
        name: skill::NAME.to_owned(),
        description: skills.description().to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "enum": skills.names(),
                    "description": "The skill to load.",
                },
            },
            "required": ["name"],
            "additionalProperties": false,
        }),
    }
}

fn warn_skipped(session: &SessionId, problems: usize) {
    if problems > 0 {
        warn!(
            session = %session,
            problems,
            "some definition files were skipped; run `coda agents lint` to see why"
        );
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain data, so a panic elsewhere cannot leave it invalid.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Anthropic => ANTHROPIC_KEY,
        BackendKind::OpenaiCompat => OPENAI_COMPAT_KEY,
        _ => FAUX_KEY,
    }
}

fn log_failure(stored: Result<(), HubError>) {
    if let Err(e) = stored {
        warn!(error = %e, "could not store an event");
    }
}

async fn publish_kind(
    hub: &Arc<Hub>,
    session: &SessionId,
    turn: &TurnId,
    kind: &str,
    body: &Value,
) -> Result<(), HubError> {
    let record = EventRecord {
        kind: kind.to_owned(),
        body: body.to_string(),
    };
    hub.publish(session, turn, record).await.map(|_| ())
}

/// What a turn event is stored as in the session's log.
#[derive(Debug, PartialEq)]
enum Row {
    /// One plain row of this kind and body, in the session's own log and
    /// wrapped in a child's alike.
    Stored { kind: &'static str, body: Value },
    /// No plain row. [`publish_turn_event`] stores some of these in a form of
    /// their own; a child's run stores none of them.
    NotStored,
}

/// The row of `event`. Long texts are kept apart in `originals`. Every kind of
/// event is named, so a new one has to be placed here before the daemon builds.
async fn event_row(hub: &Arc<Hub>, session: &SessionId, event: &TurnEvent) -> Row {
    match event {
        TurnEvent::Assistant {
            text,
            tool_calls,
            reasoning,
        } => {
            let mut kept = Vec::with_capacity(tool_calls.len());
            for call in tool_calls {
                kept.push((call, keep_original(hub, session, &call.arguments).await));
            }
            // A reply that had reasoning always says so, even when the reasoning
            // could not be kept, so a rebuilt history counts the same messages.
            let reasoning = if reasoning.is_empty() {
                None
            } else {
                let kept = keep_original(hub, session, &json!(reasoning).to_string()).await;
                Some(kept.unwrap_or_else(empty_text_id))
            };
            Row::Stored {
                kind: ASSISTANT_MESSAGE_KIND,
                body: assistant_body_with_reasoning(text, &kept, reasoning.as_ref()),
            }
        }
        TurnEvent::ToolFinished {
            tool,
            call_id,
            text,
            is_error,
        } => {
            let original = keep_original(hub, session, text).await;
            Row::Stored {
                kind: TOOL_FINISHED_KIND,
                body: tool_finished_body(tool, call_id, original.as_ref(), *is_error),
            }
        }
        TurnEvent::Steered(text) => Row::Stored {
            kind: USER_MESSAGE_KIND,
            body: json!({ "text": text, STEERED_FIELD: true }),
        },
        TurnEvent::ContextOverflow => Row::Stored {
            kind: CONTEXT_OVERFLOW_KIND,
            body: json!({}),
        },
        TurnEvent::CallRecovered { call_id, tool, how } => Row::Stored {
            kind: CALL_RECOVERED_KIND,
            body: json!(CallRecoveredBody {
                call_id: call_id.clone(),
                tool: tool.clone(),
                how: how.as_str().to_owned()
            }),
        },
        TurnEvent::TodoUpdated { items } => Row::Stored {
            kind: TODO_UPDATED_KIND,
            body: json!(TodoUpdatedBody {
                items: items.clone()
            }),
        },
        TurnEvent::ModelFallback { from, to, reason } => Row::Stored {
            kind: MODEL_FALLBACK_KIND,
            body: json!(ModelFallbackBody {
                from: from.clone(),
                to: to.clone(),
                reason: reason.clone()
            }),
        },
        TurnEvent::Text(_)
        | TurnEvent::Usage(_)
        | TurnEvent::Context { .. }
        | TurnEvent::SessionStarted
        | TurnEvent::HookBlocked { .. }
        | TurnEvent::GuardContinued { .. }
        | TurnEvent::GuardStopped { .. }
        | TurnEvent::Compaction { .. }
        | TurnEvent::Fence(_)
        | TurnEvent::Child { .. } => Row::NotStored,
    }
}

/// The pull request address in the output of a `gh pr create`, taken only when it
/// is on the session folder's `origin` host. Without a readable origin
/// nothing is taken, so output from another host or repository is never linked.
async fn pull_request_of_origin(
    hub: &Arc<Hub>,
    session: &SessionId,
    output: String,
) -> Option<String> {
    let id = session.clone();
    let record = hub
        .with_store(move |store| store.load_session(&id))
        .await
        .ok()?;
    let found = tokio::task::spawn_blocking(move || {
        let repo = origin_host(&ShipTools::default(), std::path::Path::new(&record.cwd))?;
        printed_pull_request_of(&output, &repo)
    })
    .await;
    found.ok().flatten()
}

/// Answer text the loop streamed that is not stored yet. Consecutive chunks
/// within [`DELTA_WINDOW`] become one row, as for a backend turn, so a long
/// answer is not one row per token. Any other event flushes the text first, so
/// the log keeps the order the loop produced.
#[derive(Default)]
struct StreamedText {
    /// What the turn's calls work on, until each call finishes.
    summaries: HashMap<String, Option<String>>,
    /// Calls of the shell tool that run `gh pr create`, until each finishes.
    pr_calls: HashSet<String>,
    text: String,
    /// When the oldest unstored chunk was held; `None` while nothing is held.
    since: Option<tokio::time::Instant>,
    /// How full the context was at the call whose usage comes next.
    context: Option<ContextFill>,
}

impl StreamedText {
    async fn publish(
        &mut self,
        hub: &Arc<Hub>,
        session: &SessionId,
        turn: &TurnId,
        event: TurnEvent,
    ) {
        if let TurnEvent::Context {
            used_tokens,
            window_tokens,
        } = event
        {
            self.context = Some(ContextFill {
                used_tokens,
                window_tokens,
            });
            return;
        }
        let TurnEvent::Text(chunk) = event else {
            self.flush(hub, session, turn).await;
            if let Some(output) = self.opened_pull_request(&event)
                && let Some(url) = pull_request_of_origin(hub, session, output).await
            {
                let session = session.clone();
                let linked = hub
                    .with_store_mut(move |store| store.link_pull_request(&session, &url))
                    .await;
                log_failure(linked);
            }
            let calls = self.tool_calls(session, turn, &event);
            let context = self.context.take();
            publish_turn_event(hub, session, turn, event, context).await;
            for call in calls {
                log_failure(store_event(hub, session, turn, &call).await);
            }
            return;
        };
        self.since.get_or_insert_with(tokio::time::Instant::now);
        self.text.push_str(&chunk);
        if self.text.len() >= MAX_DELTA_CHUNK_BYTES {
            self.flush(hub, session, turn).await;
        }
    }

    /// The output of a `gh pr create` the model ran that has just ended without a failure.
    fn opened_pull_request(&mut self, event: &TurnEvent) -> Option<String> {
        match event {
            TurnEvent::Assistant { tool_calls, .. } => {
                for made in tool_calls.iter().filter(|made| made.name == SHELL_TOOL) {
                    let command = serde_json::from_str::<Value>(&made.arguments)
                        .ok()
                        .and_then(|input| input.get("command")?.as_str().map(str::to_owned));
                    if command.is_some_and(|command| creates_pull_request(&command)) {
                        self.pr_calls.insert(made.id.clone());
                    }
                }
                None
            }
            TurnEvent::ToolFinished {
                call_id,
                is_error,
                text,
                ..
            } if self.pr_calls.remove(call_id) && !is_error => {
                ended_cleanly(text).then(|| text.clone())
            }
            _ => None,
        }
    }

    /// The `tool_call` events that go with `event`: a start for each call the model
    /// made, and the finish of a call that ended.
    fn tool_calls(&mut self, session: &SessionId, turn: &TurnId, event: &TurnEvent) -> Vec<Event> {
        let call = |id: &str, name: &str, summary: Option<String>, status| Event::ToolCall {
            session_id: session.clone(),
            turn_id: turn.clone(),
            id: id.to_owned(),
            name: name.to_owned(),
            summary,
            status,
        };
        match event {
            TurnEvent::Assistant { tool_calls, .. } => tool_calls
                .iter()
                .map(|made| {
                    let summary = serde_json::from_str(&made.arguments)
                        .ok()
                        .and_then(|input: Value| tool_summary(&input));
                    self.summaries.insert(made.id.clone(), summary.clone());
                    call(&made.id, &made.name, summary, ToolCallStatus::Started)
                })
                .collect(),
            TurnEvent::ToolFinished {
                tool,
                call_id,
                is_error,
                ..
            } => {
                let summary = self.summaries.remove(call_id).flatten();
                let status = if *is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Succeeded
                };
                vec![call(call_id, tool, summary, status)]
            }
            _ => Vec::new(),
        }
    }

    /// Resolves when the held text has waited out the window; never while none is held.
    async fn due(&self) {
        match self.since {
            Some(since) => tokio::time::sleep_until(since + DELTA_WINDOW).await,
            None => std::future::pending().await,
        }
    }

    async fn flush(&mut self, hub: &Arc<Hub>, session: &SessionId, turn: &TurnId) {
        self.since = None;
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        publish_turn_event(hub, session, turn, TurnEvent::Text(text), None).await;
    }
}

async fn publish_turn_event(
    hub: &Arc<Hub>,
    session: &SessionId,
    turn: &TurnId,
    event: TurnEvent,
    context: Option<ContextFill>,
) {
    if let Row::Stored { kind, body } = event_row(hub, session, &event).await {
        log_failure(publish_kind(hub, session, turn, kind, &body).await);
        return;
    }
    let stored = match event {
        TurnEvent::Text(text) => {
            let delta = Event::StreamDelta {
                session_id: session.clone(),
                turn_id: turn.clone(),
                text,
            };
            store_event(hub, session, turn, &delta).await
        }
        TurnEvent::Usage(used) => {
            let usage = Event::Usage {
                session_id: session.clone(),
                turn_id: turn.clone(),
                usage: TokenUsage {
                    input_tokens: used.input_tokens,
                    output_tokens: used.output_tokens,
                    ..TokenUsage::default()
                },
                context,
            };
            store_event(hub, session, turn, &usage).await
        }
        TurnEvent::Fence(reached) => {
            reached.confirm();
            return;
        }
        TurnEvent::Child { agent, event } => {
            task::publish_child_event(hub, session, turn, &agent, *event).await;
            return;
        }
        TurnEvent::SessionStarted => {
            publish_kind(hub, session, turn, SESSION_STARTED_KIND, &json!({})).await
        }
        TurnEvent::HookBlocked { reason } => {
            let body = json!(HookBlockedBody { reason });
            publish_kind(hub, session, turn, HOOK_BLOCKED_KIND, &body).await
        }
        TurnEvent::GuardContinued {
            instruction,
            attempt,
        } => {
            let body = json!(GuardContinueBody {
                text: instruction,
                attempt
            });
            publish_kind(hub, session, turn, GUARD_CONTINUE_KIND, &body).await
        }
        TurnEvent::GuardStopped {
            instruction,
            attempts,
        } => {
            let body = json!(GuardLimitBody {
                text: instruction,
                attempts
            });
            publish_kind(hub, session, turn, GUARD_LIMIT_KIND, &body).await
        }
        TurnEvent::Compaction {
            summary,
            replaced,
            kept,
            ack,
        } => {
            let Some(summary) = keep_original(hub, session, &summary).await else {
                ack.confirm(false);
                return;
            };
            let compaction = Event::Compaction {
                session_id: session.clone(),
                turn_id: turn.clone(),
                summary,
                replaced_messages: u32::try_from(replaced).unwrap_or(u32::MAX),
                kept_messages: u32::try_from(kept).unwrap_or(u32::MAX),
            };
            let stored = store_event(hub, session, turn, &compaction).await;
            ack.confirm(stored.is_ok());
            stored
        }
        // Stored above, as plain rows, or held by `StreamedText` for the usage that follows.
        TurnEvent::Context { .. }
        | TurnEvent::Assistant { .. }
        | TurnEvent::Steered(_)
        | TurnEvent::ToolFinished { .. }
        | TurnEvent::ContextOverflow
        | TurnEvent::ModelFallback { .. }
        | TurnEvent::TodoUpdated { .. }
        | TurnEvent::CallRecovered { .. } => return,
    };
    log_failure(stored);
}

/// Keeps a tool call's arguments or result in `originals`, apart from the event
/// log; without it a rebuilt history shows a note or an empty value instead.
async fn keep_original(hub: &Arc<Hub>, session: &SessionId, text: &str) -> Option<OriginalId> {
    if text.is_empty() {
        return Some(empty_text_id());
    }
    let (id, text) = (session.clone(), text.to_owned());
    match hub
        .with_store_mut(move |store| store.store_original(&id, &text))
        .await
    {
        Ok(original) => Some(original),
        Err(e) => {
            warn!(session = %session, error = %e, "could not keep text in the originals");
            None
        }
    }
}

/// Denies the prompts the turn left open, each with a timeout event, since
/// nobody can answer them once the turn is over.
async fn expire_open_prompts(hub: &Arc<Hub>, session: &SessionId, turn: &TurnId) {
    let id = session.clone();
    let open = match hub.with_store(move |store| store.list_pending(&id)).await {
        Ok(open) => open,
        Err(e) => {
            warn!(session = %session, error = %e, "could not list open prompts");
            return;
        }
    };
    for pending in open {
        let event = Event::Timeout {
            session_id: session.clone(),
            request_id: pending.id.clone(),
            tool: pending.tool.clone(),
        };
        let body = serde_json::to_value(&event).unwrap_or(Value::Null);
        // Event first: a failure leaves the prompt open for the turn's own cleanup.
        let stored = publish_kind(hub, session, turn, TIMEOUT_KIND, &body).await;
        if stored.is_err() {
            log_failure(stored);
            continue;
        }
        match hub
            .resolve_pending(&pending.id, PermissionDecision::Deny)
            .await
        {
            Ok(()) | Err(HubError::Store(StoreError::AlreadyResolved { .. })) => {}
            Err(e) => warn!(request = %pending.id, error = %e, "could not expire a prompt"),
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
