// SPDX-License-Identifier: AGPL-3.0-only

//! Runs the sub-agents the `task` tool names, as child conversations of a
//! model-API session.
//!
//! A child starts fresh: its system prompt is the agent's body, its tools are
//! the agent's list (never more than the parent has, and never `task`), and it
//! runs on the model its tier routes to. Its permission prompts go through the
//! parent session's broker under the agent's name, and an "always allow"
//! answered for one lasts for that run only.
//!
//! The child's events are stored under the parent session, each wrapped in a
//! `child_event`. History replay skips that kind, so rebuilding the parent
//! shows the `task` call and its result and nothing else. Storing them in the
//! parent keeps one log per session, needs no schema change and keeps child
//! runs out of the session list. They travel up the parent's own event channel,
//! so they are stored after the parent's call that started the child and before
//! that call's result.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use coda_agents::{
    Catalog, DispatchError, Report, Status, StoredTrust, TaskResult, Tier, child_tools,
    parse_report,
};
use coda_config::config::Config;
use coda_config::hooks::HookEvent;
use coda_core::broker::RunGrants;
use coda_core::history::CHILD_EVENT_KIND;
use coda_core::hooks::HookRunner;
use coda_core::prompt::{Conversation, Message, Prefix, PromptError};
use coda_core::task::{TaskFuture, TaskOutcome, TaskRequest, TaskRunner};
use coda_core::tools::{BaseToolsConfig, StoreOriginals, ToolsError, base_tools};
use coda_core::turn::{
    CancelToken, StopCondition, ToolSet, TurnConfig, TurnError, TurnEvent, TurnLoop,
};
use coda_protocol::paths::repo_identity;
use coda_protocol::types::{BackendKind, PermissionDecision, SessionId, TurnId};
use coda_providers::faux::FauxProvider;
use coda_providers::gate::PermissionGate;
use coda_providers::model::Usage;
use coda_providers::registry::{ModelRegistry, ModelTarget, RegistryError};
use coda_providers::tool_request::ToolRequest;
use coda_store::SessionRecord;
use coda_tools::task::{DepthExceeded, child_depth};
use serde_json::json;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::warn;

use super::{
    COMPACT_THRESHOLD_BYTES, EnvReader, Provider, Row, event_row, log_failure, publish_kind,
};
use crate::gate::BrokerGate;
use crate::subscribe::{Hub, HubError};

/// What every session's runner shares: the daemon's configuration, providers
/// and store.
pub(super) struct ChildEnv {
    pub(super) hub: Arc<Hub>,
    pub(super) config: Config,
    pub(super) registry: ModelRegistry,
    pub(super) env: Vec<(OsString, OsString)>,
    pub(super) key_reader: EnvReader,
    /// `CODA_HOME`, where the user's definitions live; `None` when the daemon
    /// cannot tell, in which case only the built-in agents are offered.
    pub(super) home: Option<PathBuf>,
}

impl ChildEnv {
    /// The definitions a session in `cwd` may use: the built-in ones, the
    /// user's, and the project's once its repository is trusted. Read afresh on
    /// every call, so an edited file is checked against the trusted digest again.
    /// When the trust decision cannot be read, the built-in agents are all there is.
    pub(super) async fn catalog(&self, cwd: &str) -> Catalog {
        match self.read_catalog(cwd).await {
            Ok(catalog) => catalog,
            Err(error) => {
                warn!(%error, "the definitions could not be read; only the built-in agents are offered");
                Catalog::builtin()
            }
        }
    }

    /// Like [`ChildEnv::catalog`], but a failed read is the caller's to handle.
    ///
    /// The store is held only to read the trust decision. The files are read
    /// after, so a large or slow definitions folder never keeps the other
    /// users of the store waiting.
    pub(super) async fn read_catalog(&self, cwd: &str) -> Result<Catalog, HubError> {
        let Some(home) = self.home.clone() else {
            return Ok(Catalog::builtin());
        };
        let root = repo_identity(Path::new(cwd));
        let trusted_root = root.clone();
        let trust = self
            .hub
            .with_store(move |store| StoredTrust::read(store, trusted_root.as_deref()))
            .await?;
        let catalog = tokio::task::spawn_blocking(move || {
            Catalog::load_with_trust(&home, root.as_deref(), &trust)
        })
        .await?;
        Ok(catalog)
    }
}

/// Why a `task` call could not run its agent. The text goes to the parent model.
#[derive(Debug, thiserror::Error)]
enum ChildError {
    #[error(transparent)]
    Depth(#[from] DepthExceeded),
    #[error(transparent)]
    Dispatch(#[from] DispatchError),
    #[error("the model for the agent's tier cannot be used: {0}")]
    Model(#[from] RegistryError),
    #[error(transparent)]
    Tools(#[from] ToolsError),
    #[error(transparent)]
    Prompt(#[from] PromptError),
}

/// The model a child of tier `tier` runs on: the alias `[routing]` names for
/// the tier, else the model the parent session runs.
pub(super) fn child_target(
    config: &Config,
    registry: &ModelRegistry,
    backend: BackendKind,
    stored_model: Option<&str>,
    tier: Tier,
) -> Result<ModelTarget, RegistryError> {
    match config.model_alias_for(tier) {
        Some(alias) => registry.alias(alias, backend),
        None => registry.select(backend, stored_model),
    }
}

/// How a child's run ended, as the parent is told.
pub(super) fn status_of(ended: &Result<StopCondition, TurnError>) -> Status {
    match ended {
        Ok(StopCondition::EndTurn) => Status::Completed,
        Ok(StopCondition::Cancelled) => Status::Cancelled,
        _ => Status::Failed,
    }
}

fn summary_only(summary: String) -> Report {
    Report {
        summary,
        findings: Vec::new(),
    }
}

/// What a loop is made of, apart from how deep it runs.
pub(super) struct LoopParts<G> {
    pub(super) provider: Provider,
    pub(super) window: Option<u32>,
    pub(super) gate: G,
    pub(super) tools: ToolSet,
    pub(super) conversation: Conversation,
    pub(super) config: TurnConfig,
    /// Where the loop's own events go.
    pub(super) events: UnboundedSender<TurnEvent>,
    /// A signal to share, as a child shares its parent's.
    pub(super) cancel: Option<CancelToken>,
}

/// Builds the loops of one session: its own and each child's. Every loop is
/// built here, so none can start a sub-agent past the depth cap, and only a
/// loop built here is given a [`ChildRunner`].
pub(super) struct LoopFactory {
    env: Arc<ChildEnv>,
    record: SessionRecord,
    /// The session's own gate, which carries the child's prompts to its client.
    gate: Arc<BrokerGate>,
    /// The session's scripted provider, which its children draw turns from in
    /// call order; `None` for a real provider, which each child builds anew.
    faux: Option<Arc<FauxProvider>>,
    /// The session's event channel. A child's events are sent here, so they are
    /// stored in order with the session's own.
    events: UnboundedSender<TurnEvent>,
}

impl LoopFactory {
    /// The factory for the session `record`, whose children's events go up `events`.
    pub(super) fn new(
        env: Arc<ChildEnv>,
        record: SessionRecord,
        gate: Arc<BrokerGate>,
        faux: Option<Arc<FauxProvider>>,
        events: UnboundedSender<TurnEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            env,
            record,
            gate,
            faux,
            events,
        })
    }

    /// Builds the loop of a session `depth` levels down. It is given a runner
    /// for its own children only when a child of it would be within the depth cap.
    pub(super) fn build_loop<G: PermissionGate>(
        self: &Arc<Self>,
        depth: u8,
        parts: LoopParts<G>,
    ) -> TurnLoop<Provider, G> {
        let below = child_depth(depth).is_ok().then(|| {
            Arc::new(ChildRunner {
                factory: Arc::clone(self),
                parent_tools: parts.tools.names(),
                depth,
            })
        });
        let mut turn_loop = TurnLoop::new(
            parts.provider,
            parts.gate,
            parts.tools,
            self.env.hub.store_handle(),
            parts.conversation,
            parts.config,
            parts.events,
        )
        .with_context_window(parts.window)
        .with_hooks(self.hooks_for(depth));
        if let Some(runner) = below {
            turn_loop = turn_loop.with_tasks(runner);
        }
        match parts.cancel {
            Some(cancel) => turn_loop.with_cancel_token(cancel),
            None => turn_loop,
        }
    }

    /// The user's hooks for a loop `depth` levels down, with the allowlisted
    /// environment bash gets and no API key. Every loop runs the tool hooks, so
    /// a sub-agent is no way around a blocking one. Only the session itself
    /// runs `session_start`, which marks the start of the session, not of a
    /// sub-agent.
    fn hooks_for(&self, depth: u8) -> HookRunner {
        let runner = HookRunner::new(&self.env.config.hooks, self.env.env.clone());
        if depth == 0 {
            runner
        } else {
            runner.without(HookEvent::SessionStart)
        }
    }
}

/// Runs the `task` calls of one loop: how deep it is, and the tools that bound
/// what a child of it may be given.
struct ChildRunner {
    factory: Arc<LoopFactory>,
    /// The tools of the loop this runner serves.
    parent_tools: Vec<&'static str>,
    /// How deep the loop this runner serves is: 0 for a top-level session.
    depth: u8,
}

impl ChildRunner {
    async fn run_child(&self, request: TaskRequest) -> Result<TaskOutcome, ChildError> {
        let LoopFactory { env, record, .. } = &*self.factory;
        let depth = child_depth(self.depth)?;
        let catalog = env.catalog(&record.cwd).await;
        let entry = catalog.dispatchable(&request.agent)?;
        let def = &entry.def;
        let (provider, window) = self.provider(def.tier)?;
        let tools = base_tools(
            &BaseToolsConfig {
                workdir: PathBuf::from(&record.cwd),
                bash_timeout: env.config.timing.tool_execution,
                env: env.env.clone(),
            },
            env.hub.store_handle(),
            StoreOriginals::new(env.hub.store_handle(), record.id.clone()),
        )?
        .narrowed(&child_tools(def, &self.parent_tools));
        let prefix = Prefix::new(&def.body, tools.schemas, "")?;
        let (sender, events) = unbounded_channel();
        let gate = ChildGate {
            parent: Arc::clone(&self.factory.gate),
            agent: def.name.clone(),
            grants: RunGrants::default(),
        };
        let mut child = self.factory.build_loop(
            depth,
            LoopParts {
                provider,
                window,
                gate,
                tools: tools.tools,
                conversation: Conversation::new(prefix),
                config: TurnConfig {
                    session: record.id.clone(),
                    cwd: PathBuf::from(&record.cwd),
                    max_turns: request.max_turns,
                    token_limit: request.budget,
                    compact_threshold_bytes: COMPACT_THRESHOLD_BYTES,
                },
                events: sender,
                cancel: Some(request.cancel),
            },
        );
        let mut outcome = self
            .drive(&mut child, events, &def.name, &request.prompt)
            .await;
        if let Some(held_back) = catalog.pending_override(&def.name) {
            let waiting = DispatchError::NotTrusted {
                name: def.name.clone(),
                reason: held_back.reason,
            };
            let ran = entry.source.as_str();
            outcome.text = format!(
                "{}\nnote: {waiting}. The {ran} agent ran instead.",
                outcome.text
            );
        }
        Ok(outcome)
    }

    /// The provider for a child of `tier` and its model's context window.
    fn provider(&self, tier: Tier) -> Result<(Provider, Option<u32>), ChildError> {
        let LoopFactory {
            env, record, faux, ..
        } = &*self.factory;
        if let Some(faux) = faux {
            return Ok((Provider::Faux(Arc::clone(faux)), None));
        }
        let target = child_target(
            &env.config,
            &env.registry,
            record.backend,
            record.model.as_deref(),
            tier,
        )?;
        let reader = Arc::clone(&env.key_reader);
        let client = env.registry.build(&target, move |name| reader(name))?;
        Ok((Provider::Api(client), env.registry.context_window(&target)))
    }

    /// Runs the child to the end, storing its events as it goes.
    async fn drive(
        &self,
        child: &mut TurnLoop<Provider, ChildGate>,
        mut events: UnboundedReceiver<TurnEvent>,
        agent: &str,
        prompt: &str,
    ) -> TaskOutcome {
        let mut used = Usage {
            input_tokens: 0,
            output_tokens: 0,
        };
        let ended = {
            let send = child.send(prompt);
            tokio::pin!(send);
            loop {
                tokio::select! {
                    ended = &mut send => break ended,
                    Some(event) = events.recv() => self.keep(agent, event, &mut used),
                }
            }
        };
        while let Ok(event) = events.try_recv() {
            self.keep(agent, event, &mut used);
        }
        let answer = match child.conversation().messages().last() {
            Some(Message::Assistant { text, .. }) => text.clone(),
            _ => String::new(),
        };
        let status = status_of(&ended);
        // A run that did not complete leaves no report to check, only what it said last.
        let report = match (&ended, status) {
            (_, Status::Completed) => parse_report(&answer),
            (Ok(stop), _) => Ok(summary_only(format!(
                "stopped early ({}): {answer}",
                stop.as_str()
            ))),
            (Err(error), _) => Ok(summary_only(format!("failed: {error}"))),
        };
        let (text, is_error) = match report {
            Ok(report) => (
                TaskResult { status, report }.render(agent),
                status != Status::Completed,
            ),
            Err(error) => (error.render(agent), true),
        };
        TaskOutcome {
            text,
            is_error,
            usage: used,
        }
    }

    fn keep(&self, agent: &str, event: TurnEvent, used: &mut Usage) {
        match event {
            // The parent counts and stores the child's total once, as the
            // result of the `task` call.
            TurnEvent::Usage(usage) => {
                used.input_tokens = used.input_tokens.saturating_add(usage.input_tokens);
                used.output_tokens = used.output_tokens.saturating_add(usage.output_tokens);
            }
            // The child's loop waits for this answer before it swaps in the summary.
            // Nothing rebuilds a child from stored events, so there is nothing to
            // store and the answer is yes.
            TurnEvent::Compaction { ack, .. } => ack.confirm(true),
            TurnEvent::Text(_) => {}
            other => {
                // A closed channel means nobody is watching the session.
                let _ = self.factory.events.send(TurnEvent::Child {
                    agent: agent.to_owned(),
                    event: Box::new(other),
                });
            }
        }
    }
}

impl TaskRunner for ChildRunner {
    fn run(&self, request: TaskRequest) -> TaskFuture<'_> {
        Box::pin(async move {
            self.run_child(request)
                .await
                .unwrap_or_else(|error| TaskOutcome {
                    text: format!("the task could not run: {error}"),
                    is_error: true,
                    usage: Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                    },
                })
        })
    }
}

/// Stores one event of a child's run under the parent session, wrapped with
/// the agent's name and the kind and body it would have had in its own log.
pub(super) async fn publish_child_event(
    hub: &Arc<Hub>,
    session: &SessionId,
    turn: &TurnId,
    agent: &str,
    event: TurnEvent,
) {
    // The rest are not stored: the parent counts the usage, and a child's own
    // children send their events up the session's channel, not through this
    // one. A child runs no `session_start` hook, so it never reports one blocked.
    let Row::Stored { kind, body } = event_row(hub, session, &event).await else {
        return;
    };
    let wrapped = json!({ "agent": agent, "kind": kind, "body": body });
    log_failure(publish_kind(hub, session, turn, CHILD_EVENT_KIND, &wrapped).await);
}

/// Decides a child's tool calls through the parent session's gate, with the
/// agent's name on any prompt. An "always allow" is kept in this run's grants,
/// which the broker checks under the same policy as stored grants.
struct ChildGate {
    parent: Arc<BrokerGate>,
    agent: String,
    grants: RunGrants,
}

impl PermissionGate for ChildGate {
    async fn request(&self, request: &ToolRequest) -> PermissionDecision {
        self.parent
            .request_for_agent(request, &self.agent, &self.grants)
            .await
    }
}
