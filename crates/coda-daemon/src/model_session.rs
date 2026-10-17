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

use coda_config::config::{Config, ProviderConfig};
use coda_core::prompt::{Conversation, Prefix, PromptError};
use coda_core::tools::{BaseToolsConfig, StoreOriginals, ToolsError, base_tools};
use coda_core::turn::{CancelToken, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::events::Event;
use coda_protocol::types::{BackendKind, PermissionDecision, SessionId, TokenUsage, TurnId};
use coda_providers::anthropic::provider::{AnthropicOptions, AnthropicProvider};
use coda_providers::anthropic::request::RequestSettings;
use coda_providers::faux::{FauxError, FauxProvider};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{
    ModelProvider, ModelRequest, ModelStream, ProviderError, SystemClock, TokioSleeper,
};
use coda_providers::tool_request::ToolRequest;
use coda_store::{EventRecord, SessionRecord, StoreError};
use coda_tools::bash::{DEFAULT_ENV_ALLOWLIST, allowlisted_env};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tracing::{Instrument, info, warn};

use crate::gate::{GateConfig, PermissionAnswers, SessionGate};
use crate::subscribe::{Hub, HubError};
use crate::trace::{backend_span, permission_span};
use crate::turn::{TurnError, Turns, permission_mode, record_decision, store_event};

const FAUX_KEY: &str = "faux";
const ANTHROPIC_KEY: &str = "anthropic";
const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-5-5";
const MAX_OUTPUT_TOKENS: u32 = 8192;
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

pub const TOOL_FINISHED_KIND: &str = "tool_finished";
pub const CONTEXT_OVERFLOW_KIND: &str = "context_overflow";
pub const TURN_STOPPED_KIND: &str = "turn_stopped";
/// A session whose earlier conversation was lost starts over from here.
pub const CONVERSATION_RESET_KIND: &str = "conversation_reset";
const TIMEOUT_KIND: &str = "timeout";

/// Why a model-API session could not be started.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("provider {0} is disabled; set providers.{0}.enabled = true to use it")]
    Disabled(&'static str),
    #[error("backend {0:?} is not a model-API backend")]
    NotAModelBackend(BackendKind),
    #[error(transparent)]
    Faux(#[from] FauxError),
    #[error("the provider cannot be used: {0}")]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tools(#[from] ToolsError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
}

/// The provider a session was configured with.
enum Provider {
    Faux(FauxProvider),
    Anthropic(AnthropicProvider<TokioSleeper, SystemClock>),
    /// Fails the task that drives it, to exercise panic handling.
    #[cfg(test)]
    Panicking,
}

impl ModelProvider for Provider {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProviderError> {
        match self {
            Self::Faux(provider) => provider.stream(request).await,
            Self::Anthropic(provider) => provider.stream(request).await,
            #[cfg(test)]
            Self::Panicking => panic!("the test provider panics on purpose"),
        }
    }
}

/// Answers a loop's permission requests through the daemon's prompt flow.
struct ModelGate {
    hub: Arc<Hub>,
    answers: Arc<PermissionAnswers>,
    config: GateConfig,
    turn: Arc<Mutex<TurnId>>,
}

impl PermissionGate for ModelGate {
    async fn request(&self, request: &ToolRequest) -> PermissionDecision {
        let turn = lock(&self.turn).clone();
        let gate = SessionGate::new(
            Arc::clone(&self.hub),
            GateConfig {
                turn,
                ..self.config.clone()
            },
            Arc::clone(&self.answers),
        );
        let span = permission_span(&self.config.session, request.tool.name());
        let decision = gate.request(request).instrument(span.clone()).await;
        record_decision(&span, decision);
        decision
    }
}

/// One session's loop with the channel its events arrive on.
struct ModelLoop {
    turn_loop: TurnLoop<Provider, ModelGate>,
    events: UnboundedReceiver<TurnEvent>,
    turn: Arc<Mutex<TurnId>>,
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
        let armed = self.sessions.running().remove(&self.session).flatten();
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
    answers: Arc<PermissionAnswers>,
    config: Config,
    env: Vec<(OsString, OsString)>,
    key_reader: EnvReader,
    /// Present from the moment a send reserves the session, with its cancel
    /// token and turn once it runs; never held across an await.
    running: Mutex<HashMap<SessionId, Option<(CancelToken, TurnId)>>>,
    /// Idle conversations, least recently used first.
    loops: Mutex<Vec<(SessionId, ModelLoop)>>,
}

impl ModelSessions {
    /// Sessions that read API keys through `key_reader` and keep only the
    /// allowlisted part of `env`.
    #[must_use]
    pub fn new(
        hub: Arc<Hub>,
        answers: Arc<PermissionAnswers>,
        config: Config,
        env: Vec<(OsString, OsString)>,
        key_reader: EnvReader,
    ) -> Arc<Self> {
        Arc::new(Self {
            hub,
            answers,
            config,
            env: allowlisted_env(DEFAULT_ENV_ALLOWLIST, env),
            key_reader,
            running: Mutex::default(),
            loops: Mutex::default(),
        })
    }

    /// Starts a send with `text` and returns without waiting for it to end.
    ///
    /// # Errors
    /// Returns [`TurnError::Busy`] when the session is running,
    /// [`TurnError::Model`] when its provider or tools cannot be set up, and
    /// [`TurnError::Hub`] when the store refuses.
    pub(crate) async fn send(
        self: &Arc<Self>,
        record: SessionRecord,
        text: String,
    ) -> Result<TurnId, TurnError> {
        let session = record.id.clone();
        let slot = self.reserve(&session)?;
        info!(session = %session, step = "build_loop", "preparing the model session");
        let (model_loop, rebuilt) = match self.take_loop(&session) {
            Some(model_loop) => (model_loop, false),
            None => (self.build(&record)?, self.has_history(&session).await),
        };
        info!(session = %session, step = "begin_turn", "recording the user message");
        let turn = match Turns::begin(&self.hub, &session, &text).await {
            Ok(turn) => turn,
            Err(e) => {
                self.keep_loop(session, model_loop);
                return Err(e.into());
            }
        };
        if rebuilt {
            let body = json!({});
            let reset = publish_kind(&self.hub, &session, &turn, CONVERSATION_RESET_KIND, &body);
            log_failure(reset.await);
        }
        self.keep_prompt(&session, &text).await;
        *lock(&model_loop.turn) = turn.clone();
        // Reset before arming: a cancel from here on must stop this send.
        let cancel = model_loop.turn_loop.cancel_token();
        cancel.reset();
        slot.arm((cancel, turn.clone()));
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

    /// Cancels the running send of `session`; `false` when none is running.
    pub fn cancel(&self, session: &SessionId) -> bool {
        match self.running().get(session) {
            Some(Some((cancel, _))) => {
                cancel.cancel();
                true
            }
            _ => false,
        }
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
        for (cancel, _) in self.running().values().flatten() {
            cancel.cancel();
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
                .filter_map(|(session, armed)| Some((session.clone(), armed.as_ref()?.1.clone())))
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

    fn running(&self) -> MutexGuard<'_, HashMap<SessionId, Option<(CancelToken, TurnId)>>> {
        lock(&self.running)
    }

    fn take_loop(&self, session: &SessionId) -> Option<ModelLoop> {
        let mut loops = lock(&self.loops);
        let at = loops.iter().position(|(id, _)| id == session)?;
        Some(loops.remove(at).1)
    }

    fn keep_loop(&self, session: SessionId, model_loop: ModelLoop) {
        let mut loops = lock(&self.loops);
        if let Some(evicted) = keep_recent(&mut loops, session, model_loop, MAX_IDLE_LOOPS) {
            info!(session = %evicted, "dropped the least recently used idle conversation");
        }
    }

    async fn has_history(&self, session: &SessionId) -> bool {
        let id = session.clone();
        let load = move |s: &coda_store::Store| s.load_events(&id, 0, 1);
        self.hub.with_store(load).await.is_ok_and(|e| !e.is_empty())
    }

    fn reserve(self: &Arc<Self>, session: &SessionId) -> Result<Slot, TurnError> {
        match self.running().entry(session.clone()) {
            std::collections::hash_map::Entry::Occupied(_) => {
                return Err(TurnError::Busy(session.clone()));
            }
            std::collections::hash_map::Entry::Vacant(vacant) => vacant.insert(None),
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
                Ok(Provider::Faux(FauxProvider::from_config(&self.config)?))
            }
            BackendKind::Anthropic => {
                let section = self.section(ANTHROPIC_KEY)?;
                let options = AnthropicOptions {
                    base_url: ANTHROPIC_BASE_URL.to_owned(),
                    settings: RequestSettings {
                        model: record
                            .model
                            .clone()
                            .unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_owned()),
                        max_tokens: MAX_OUTPUT_TOKENS,
                        system: String::new(),
                        tools: Vec::new(),
                    },
                    timing: coda_providers::anthropic::Timing::from_config(&self.config.timing),
                };
                let reader = Arc::clone(&self.key_reader);
                let lookup = move |name: &str| reader(name);
                Ok(Provider::Anthropic(AnthropicProvider::from_config(
                    section, options, lookup,
                )?))
            }
            other => Err(ModelError::NotAModelBackend(other)),
        }
    }

    fn build(&self, record: &SessionRecord) -> Result<ModelLoop, ModelError> {
        let provider = self.provider(record)?;
        let tools = base_tools(
            &BaseToolsConfig {
                workdir: PathBuf::from(&record.cwd),
                bash_timeout: self.config.timing.tool_execution,
                env: self.env.clone(),
            },
            StoreOriginals::new(self.hub.store_handle(), record.id.clone()),
        )?;
        let prefix = Prefix::new(SYSTEM_PROMPT, tools.schemas, "")?;
        let turn = Arc::new(Mutex::new(TurnId::new(NO_TURN)));
        let gate = ModelGate {
            hub: Arc::clone(&self.hub),
            answers: Arc::clone(&self.answers),
            config: GateConfig {
                session: record.id.clone(),
                turn: TurnId::new(NO_TURN),
                repo: PathBuf::from(&record.cwd),
                workspace_root: PathBuf::from(&record.cwd),
                mode: permission_mode(record.mode),
                prompt_timeout: self.config.timing.permission_default_deny,
            },
            turn: Arc::clone(&turn),
        };
        let (sender, events) = unbounded_channel();
        let turn_loop = TurnLoop::new(
            provider,
            gate,
            tools.tools,
            self.hub.store_handle(),
            Conversation::new(prefix),
            TurnConfig {
                session: record.id.clone(),
                cwd: PathBuf::from(&record.cwd),
                max_turns: self.config.max_turns,
                token_limit: self.config.token_cap,
                compact_threshold_bytes: COMPACT_THRESHOLD_BYTES,
            },
            sender,
        );
        Ok(ModelLoop {
            turn_loop,
            events,
            turn,
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
        close_turn(&self.hub, &session, &turn, failed).await;
        let model_loop = ModelLoop {
            turn_loop,
            events,
            turn: shared_turn,
        };
        self.keep_loop(session, model_loop);
    }
}

impl Slot {
    fn arm(&self, armed: (CancelToken, TurnId)) {
        self.sessions
            .running()
            .insert(self.session.clone(), Some(armed));
    }
}

/// Appends `item`, then drops the oldest entry when more than `max` are kept
/// and returns the session it belonged to.
fn keep_recent<T>(
    items: &mut Vec<(SessionId, T)>,
    session: SessionId,
    item: T,
    max: usize,
) -> Option<SessionId> {
    items.push((session, item));
    (items.len() > max).then(|| items.remove(0).0)
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain data, so a panic elsewhere cannot leave it invalid.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn backend_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Anthropic => ANTHROPIC_KEY,
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
        TurnEvent::ToolFinished { tool, is_error } => {
            let body = json!({ "tool": tool, "is_error": is_error });
            publish_kind(hub, session, turn, TOOL_FINISHED_KIND, &body).await
        }
        TurnEvent::ContextOverflow => {
            publish_kind(hub, session, turn, CONTEXT_OVERFLOW_KIND, &json!({})).await
        }
    };
    log_failure(stored);
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
