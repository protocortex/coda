// SPDX-License-Identifier: AGPL-3.0-only

//! Model-API sessions: a `coda_core` turn loop driven by a configured
//! provider, with its events stored through the hub and its permission
//! prompts answered through the daemon's permission flow.
//!
//! A session keeps its loop across sends; a send runs on its own task, with
//! the loop out of the table until the send ends.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use coda_agents::{SKILL_TOOL_NAME, SkillTool};
use coda_config::config::{Config, Dirs, ProviderConfig};
use coda_core::broker::BrokerError;
use coda_core::history::{
    ASSISTANT_MESSAGE_KIND, Rebuild, TOOL_FINISHED_KIND, assistant_body, empty_text_id,
    tool_finished_body,
};
pub use coda_core::history::{
    CONTEXT_OVERFLOW_KIND, HOOK_BLOCKED_KIND, MODEL_SWITCHED_KIND, TURN_STOPPED_KIND,
};
use coda_core::prompt::{Conversation, Prefix, PromptError, ToolSchema};
use coda_core::tools::{
    BaseToolsConfig, StoreOriginals, ToolsError, base_tools, refresh_index, task_schema,
};
use coda_core::turn::{CancelToken, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::events::{Event, TIMEOUT_KIND};
use coda_protocol::types::{
    BackendKind, OriginalId, PermissionDecision, SessionId, TokenUsage, TurnId,
};
use coda_providers::faux::{FauxError, FauxProvider};
use coda_providers::model::{ModelProvider, ModelRequest, ModelStream, ProviderError};
use coda_providers::registry::{
    ANTHROPIC_KEY, ModelClient, ModelRegistry, OPENAI_COMPAT_KEY, RegistryError,
};
use coda_store::{EventRecord, SessionRecord, StoreError};
use coda_tools::Tool;
use coda_tools::bash::{DEFAULT_ENV_ALLOWLIST, allowlisted_env};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tracing::{Instrument, info, warn};

use crate::gate::{BrokerGate, Brokers};
use crate::idle_pool::IdlePool;
use crate::subscribe::{Hub, HubError};
use crate::trace::backend_span;
use crate::turn::{Reservation, TurnError, Turns, store_event};

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

/// One session's loop with the channel its events arrive on.
struct ModelLoop {
    turn_loop: TurnLoop<Provider, Arc<BrokerGate>>,
    events: UnboundedReceiver<TurnEvent>,
    turn: Arc<Mutex<TurnId>>,
    limits: Limits,
    /// The skills this loop offers, to tell at the next send whether the files
    /// or the trust decision changed.
    skills: Option<Arc<SkillTool>>,
}

/// What a loop takes over from the idle one it replaces: the send limits and
/// the tokens already spent, so a rebuild never hands the session a fresh budget.
#[derive(Debug, Clone, Copy)]
struct Carried {
    limits: Limits,
    used: u64,
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
}

impl Drop for Slot {
    fn drop(&mut self) {
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
    key_reader: EnvReader,
    /// Present from the moment a send reserves the session, with its cancel
    /// token and turn once it runs; never held across an await.
    running: Mutex<HashMap<SessionId, Reservation<(CancelToken, TurnId)>>>,
    /// Idle conversations, least recently used first.
    loops: IdlePool<ModelLoop>,
    /// Limits and spent tokens of conversations a rebuild dropped, until the
    /// next send builds the replacement.
    carried: Mutex<HashMap<SessionId, Carried>>,
    /// What the runners of the sessions' `task` calls share.
    children: Arc<task::ChildEnv>,
}

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
            key_reader,
            running: Mutex::default(),
            loops: IdlePool::new(MAX_IDLE_LOOPS),
            carried: Mutex::default(),
            children,
        })
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
    ) -> Result<TurnId, TurnError> {
        let session = record.id.clone();
        let slot = self.reserve(&session)?;
        // The caller's record may predate a model switch that finished while
        // this send was starting; under the reservation the stored one is final.
        let id = session.clone();
        let record = self.hub.with_store(move |s| s.load_session(&id)).await?;
        info!(session = %session, step = "build_loop", "preparing the model session");
        let model_loop = self.loop_for_send(&record, limits).await?;
        info!(session = %session, step = "begin_turn", "recording the user message");
        let turn = match Turns::begin(&self.hub, &session, &text).await {
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
        self.keep_prompt(&session, &text).await;
        let span = backend_span(&session, &turn, backend_name(record.backend));
        let sessions = Arc::clone(self);
        let task_turn = turn.clone();
        tokio::spawn(
            async move {
                let _slot = slot;
                sessions.run(model_loop, session, task_turn, text).await;
            }
            .instrument(span),
        );
        Ok(turn)
    }

    /// Switches an idle model-API session to the alias `alias`.
    ///
    /// The steps: reserve the session so no send can start, resolve the alias
    /// and build its client to prove it is usable, store the resolved model id,
    /// drop the idle loop and record the switch. The next send builds a new loop
    /// with a fresh prefix and the history rebuilt from the stored events, and
    /// keeps the send limits and spent tokens of the dropped loop. The alias name lives only in
    /// the switch event, so a later change to the aliases cannot reinterpret the
    /// stored model. Only `session` is touched.
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

    /// Cancels the running send of `session`; `false` when none is running.
    /// A send that is still starting is cancelled as soon as it is armed.
    pub fn cancel(&self, session: &SessionId) -> bool {
        match self.running().get_mut(session) {
            Some(reservation) => reservation.request_cancel(|(cancel, _)| cancel.cancel()),
            None => false,
        }
    }

    /// Drops the idle conversation of `session`; `false` when it held none.
    /// Its stored events stay, so a later send rebuilds the loop from them.
    pub(crate) fn close(&self, session: &SessionId) -> bool {
        lock(&self.carried).remove(session);
        self.take_loop(session).is_some()
    }

    /// Whether the send of `session` has emitted its terminal event and only cleans up.
    pub(crate) fn is_winding_down(&self, session: &SessionId) -> bool {
        self.running()
            .get(session)
            .is_some_and(Reservation::is_winding_down)
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

    fn running(&self) -> MutexGuard<'_, HashMap<SessionId, Reservation<(CancelToken, TurnId)>>> {
        lock(&self.running)
    }

    fn take_loop(&self, session: &SessionId) -> Option<ModelLoop> {
        self.loops.take(session)
    }

    fn keep_loop(&self, session: SessionId, model_loop: ModelLoop) {
        if let Some((evicted, _)) = self.loops.keep(session, model_loop) {
            info!(session = %evicted, "dropped the least recently used idle conversation");
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
        Ok(Slot {
            sessions: Arc::clone(self),
            session: session.clone(),
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
    /// history the session stored. An idle loop whose skills no longer match
    /// the files and the trust decision is dropped, so a revoked or edited
    /// project skill is gone by this send; the limits and spent tokens carry over.
    async fn loop_for_send(
        &self,
        record: &SessionRecord,
        limits: Limits,
    ) -> Result<ModelLoop, ModelError> {
        let skills = self.skill_tool(record).await.map(Arc::new);
        if let Some(model_loop) = self.take_loop(&record.id) {
            if model_loop.skills == skills {
                return Ok(model_loop);
            }
            info!(session = %record.id, "the skills changed; rebuilding the conversation");
            lock(&self.carried).insert(record.id.clone(), model_loop.carried());
        }
        let carried = lock(&self.carried)
            .get(&record.id)
            .copied()
            .unwrap_or(Carried { limits, used: 0 });
        let built = self.build(record, carried, skills).await?;
        lock(&self.carried).remove(&record.id);
        Ok(built)
    }

    /// The skill tool for the user's skills and, once trusted, the project's.
    /// Skills that cannot be loaded cost the session its skills, not the session.
    async fn skill_tool(&self, record: &SessionRecord) -> Option<SkillTool> {
        match self.children.catalog(&record.cwd).await {
            Ok(catalog) => {
                if !catalog.problems().is_empty() {
                    warn!(
                        session = %record.id,
                        problems = catalog.problems().len(),
                        "some definition files were skipped; run `coda agents lint` to see why"
                    );
                }
                SkillTool::from_catalog(&catalog)
            }
            Err(error) => {
                warn!(session = %record.id, %error, "could not load the skills; the session has none");
                None
            }
        }
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
    /// free for other users between pages.
    async fn stored_conversation(
        &self,
        session: &SessionId,
        prefix: Prefix,
    ) -> Result<Conversation, HubError> {
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
            match next {
                Some(last) => after = last,
                None => return Ok(rebuild.finish()),
            }
        }
    }

    async fn build(
        &self,
        record: &SessionRecord,
        Carried { limits, used }: Carried,
        skills: Option<Arc<SkillTool>>,
    ) -> Result<ModelLoop, ModelError> {
        let provider = self.provider(record)?;
        let faux = if let Provider::Faux(faux) = &provider {
            Some(Arc::clone(faux))
        } else {
            None
        };
        let (store, workdir) = (self.hub.store_handle(), PathBuf::from(&record.cwd));
        if let Err(error) =
            tokio::task::spawn_blocking(move || refresh_index(&store, &workdir)).await
        {
            warn!(%error, "the code index refresh crashed");
        }
        let mut tools = base_tools(
            &BaseToolsConfig {
                workdir: PathBuf::from(&record.cwd),
                bash_timeout: self.config.timing.tool_execution,
                env: self.env.clone(),
            },
            self.hub.store_handle(),
            StoreOriginals::new(self.hub.store_handle(), record.id.clone()),
        )?;
        if let Some(skills) = &skills {
            tools.schemas.push(skill_schema(skills));
            tools.tools = tools.tools.with(Arc::clone(skills) as Arc<dyn Tool>);
        }
        let mut schemas = tools.schemas;
        schemas.push(task_schema(&self.children.agents(&record.cwd).await));
        let prefix = Prefix::new(SYSTEM_PROMPT, schemas, "")?;
        let conversation = self.stored_conversation(&record.id, prefix).await?;
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
        let runner = task::ChildRunner::new(
            Arc::clone(&self.children),
            record.clone(),
            Arc::clone(&gate),
            faux,
            sender.clone(),
        );
        let turn_loop = runner
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
            .with_used_tokens(used);
        Ok(ModelLoop {
            turn_loop,
            events,
            turn,
            limits,
            skills,
        })
    }

    /// The named steps of a running send: drive the loop while storing its
    /// events, expire orphaned prompts, record the end.
    async fn run(
        self: Arc<Self>,
        model_loop: ModelLoop,
        session: SessionId,
        turn: TurnId,
        text: String,
    ) {
        let ModelLoop {
            mut turn_loop,
            mut events,
            turn: shared_turn,
            limits,
            skills,
        } = model_loop;
        info!(session = %session, turn = %turn, step = "drive", "driving the turn loop");
        let outcome = {
            let send = turn_loop.send(&text);
            tokio::pin!(send);
            loop {
                tokio::select! {
                    outcome = &mut send => break outcome,
                    Some(event) = events.recv() => {
                        publish_turn_event(&self.hub, &session, &turn, event).await;
                    }
                }
            }
        };
        while let Ok(event) = events.try_recv() {
            publish_turn_event(&self.hub, &session, &turn, event).await;
        }
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
        close_turn(&self.hub, &session, &turn, failed).await;
        let model_loop = ModelLoop {
            turn_loop,
            events,
            turn: shared_turn,
            limits,
            skills,
        };
        self.keep_loop(session, model_loop);
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
}

/// The schema of the skill tool: the index is its description, and only the
/// skills listed in it can be named.
fn skill_schema(skills: &SkillTool) -> ToolSchema {
    ToolSchema {
        name: SKILL_TOOL_NAME.to_owned(),
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

async fn publish_turn_event(hub: &Arc<Hub>, session: &SessionId, turn: &TurnId, event: TurnEvent) {
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
            };
            store_event(hub, session, turn, &usage).await
        }
        TurnEvent::Assistant { text, tool_calls } => {
            let mut kept = Vec::with_capacity(tool_calls.len());
            for call in &tool_calls {
                kept.push((call, keep_original(hub, session, &call.arguments).await));
            }
            let body = assistant_body(&text, &kept);
            publish_kind(hub, session, turn, ASSISTANT_MESSAGE_KIND, &body).await
        }
        TurnEvent::ToolFinished {
            tool,
            call_id,
            text,
            is_error,
        } => {
            let original = keep_original(hub, session, &text).await;
            let body = tool_finished_body(&tool, &call_id, original.as_ref(), is_error);
            publish_kind(hub, session, turn, TOOL_FINISHED_KIND, &body).await
        }
        TurnEvent::ContextOverflow => {
            publish_kind(hub, session, turn, CONTEXT_OVERFLOW_KIND, &json!({})).await
        }
        TurnEvent::Child { agent, event } => {
            task::publish_child_event(hub, session, turn, &agent, *event).await;
            return;
        }
        TurnEvent::HookBlocked { reason } => {
            publish_kind(
                hub,
                session,
                turn,
                HOOK_BLOCKED_KIND,
                &json!({ "reason": reason }),
            )
            .await
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
            warn!(session = %session, error = %e, "could not keep the text of a tool call");
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
