// SPDX-License-Identifier: AGPL-3.0-only

//! The turn loop: stream a model reply, run its tool calls behind the
//! permission gate, feed the results back, and stop for a named reason.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use coda_protocol::types::{PermissionDecision, SessionId};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{
    ModelEvent, ModelProvider, ModelRequest, ProviderError, RequestMessage, RequestToolCall,
    StopReason, ToolSchema as ModelToolSchema, Usage,
};
use coda_providers::tool_request::{Tool as RequestTool, ToolRequest};
use coda_store::Store;
use coda_telemetry::attrs::{CODA_SESSION_ID, CODA_TOOL_NAME};
use coda_tools::Tool;
use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{Instrument, info, info_span, warn};

use crate::budget::TokenBudget;
use crate::economy::{cap_ends, compact};
use crate::prompt::{Conversation, Message, ToolCall};

/// What the model is told about a call that never ran because the turn stopped.
pub(crate) const SKIPPED: &str = "not run: the turn stopped before this call";
/// Arguments assumed for a call that streamed none, as no-argument tools do.
pub(crate) const EMPTY_ARGUMENTS: &str = "{}";
/// Stored for a send that ended before the model replied, so roles keep alternating.
pub(crate) const NO_REPLY: &str = "[no reply: the turn stopped early]";
/// What the model is told about a call whose permission prompt was cancelled.
const PROMPT_CANCELLED: &str = "not run: the turn was cancelled while asking permission";
/// Name of the span around one model call.
pub const PROVIDER_CALL_SPAN: &str = "coda.provider.call";
/// Name of the span around one tool call.
pub const TOOL_CALL_SPAN: &str = "coda.tool.call";
/// Longest compaction error text shown to the model.
const MAX_ERROR_CHARS: usize = 200;

/// Why a send stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCondition {
    /// The model finished its answer.
    EndTurn,
    /// The model was called the allowed number of times and still wanted tools.
    MaxTurns,
    /// The session token budget is used up.
    BudgetExhausted,
    /// The request no longer fits the model context window.
    ContextOverflow,
    /// The user cancelled.
    Cancelled,
    /// The model declined to answer.
    Refused,
    /// The provider paused the turn.
    Paused,
    /// The model hit its output limit.
    MaxTokens,
}

impl StopCondition {
    /// The stable name clients see.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTurns => "max_turns",
            Self::BudgetExhausted => "budget_exhausted",
            Self::ContextOverflow => "context_overflow",
            Self::Cancelled => "cancelled",
            Self::Refused => "refused",
            Self::Paused => "paused",
            Self::MaxTokens => "max_tokens",
        }
    }
}

/// What a client can show while a send runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEvent {
    /// A piece of answer text.
    Text(String),
    /// A tool call finished, failed or was denied.
    ToolFinished {
        /// The tool that was called.
        tool: String,
        /// The call this answers.
        call_id: String,
        /// What the model was told.
        text: String,
        /// True when the model got an error back.
        is_error: bool,
    },
    /// The model replied; this is the assistant message added to the history.
    Assistant {
        /// The answer text.
        text: String,
        /// The calls it made.
        tool_calls: Vec<ToolCall>,
    },
    /// A model call finished and used these tokens.
    Usage(Usage),
    /// The context window is full; compaction is not available.
    ContextOverflow,
}

/// Why a send failed outright.
#[derive(Debug, thiserror::Error)]
pub enum TurnError {
    /// The provider failed in a way the loop cannot recover from.
    #[error("the model provider failed: {0}")]
    Provider(#[from] ProviderError),
    /// The response stream ended without a stop event.
    #[error("the model response ended without a stop reason")]
    StreamEnded,
}

/// Shared cancel signal for one session.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    wake: Arc<Notify>,
}

impl CancelToken {
    /// Asks the running send to stop. Cancelling twice is harmless.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }

    /// Clears a cancel; call it before a send starts, never while one runs.
    pub fn reset(&self) {
        self.flag.store(false, Ordering::SeqCst);
    }

    /// Whether a cancel was requested and not reset since. Public only so the
    /// daemon's tests can observe a token; the turn loop checks it itself.
    #[doc(hidden)]
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.flag)
    }

    async fn cancelled(&self) {
        while !self.is_cancelled() {
            self.wake.notified().await;
        }
    }
}

/// The tools a session may call, by name.
#[derive(Default)]
pub struct ToolSet {
    tools: HashMap<&'static str, Arc<dyn Tool>>,
}

impl ToolSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The names of the tools, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.tools.keys().copied().collect();
        names.sort_unstable();
        names
    }

    /// Adds `tool` under its own name.
    #[must_use]
    pub fn with(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.insert(tool.name(), tool);
        self
    }
}

/// Limits and identity of one session's loop.
#[derive(Debug, Clone)]
pub struct TurnConfig {
    /// The session the loop serves.
    pub session: SessionId,
    /// Working directory tools run in.
    pub cwd: PathBuf,
    /// Most model calls one send may make.
    pub max_turns: u32,
    /// Token limit for the whole session.
    pub token_limit: u64,
    /// Tool output above this many bytes is compacted.
    pub compact_threshold_bytes: usize,
}

/// One session's loop state.
pub struct TurnLoop<P, G> {
    provider: P,
    gate: G,
    tools: ToolSet,
    store: Arc<Mutex<Store>>,
    config: TurnConfig,
    budget: TokenBudget,
    conversation: Conversation,
    events: UnboundedSender<TurnEvent>,
    cancel: CancelToken,
}

impl<P: ModelProvider, G: PermissionGate> TurnLoop<P, G> {
    /// Builds the loop; events go to `events`.
    #[must_use]
    pub fn new(
        provider: P,
        gate: G,
        tools: ToolSet,
        store: Arc<Mutex<Store>>,
        conversation: Conversation,
        config: TurnConfig,
        events: UnboundedSender<TurnEvent>,
    ) -> Self {
        let budget = TokenBudget::new(config.token_limit);
        Self {
            provider,
            gate,
            tools,
            store,
            config,
            budget,
            conversation,
            events,
            cancel: CancelToken::default(),
        }
    }

    /// The handle that cancels a running send.
    #[must_use]
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// The conversation so far.
    #[must_use]
    pub fn conversation(&self) -> &Conversation {
        &self.conversation
    }

    /// Adds the user's message and runs the loop until it stops.
    ///
    /// A cancel that arrived before the send started stops it, so the caller
    /// resets the token first. Whatever way the send ends, the history is left
    /// ready for the next one.
    ///
    /// # Errors
    ///
    /// Returns [`TurnError::Provider`] for a provider failure that is not a
    /// context overflow, and [`TurnError::StreamEnded`] for a truncated stream.
    pub async fn send(&mut self, text: &str) -> Result<StopCondition, TurnError> {
        self.conversation.push(Message::User(text.to_owned()));
        let result = self.run().await;
        if !matches!(
            self.conversation.messages().last(),
            Some(Message::Assistant { .. })
        ) {
            self.conversation.push(Message::Assistant {
                text: NO_REPLY.to_owned(),
                tool_calls: Vec::new(),
            });
        }
        result
    }

    async fn run(&mut self) -> Result<StopCondition, TurnError> {
        for _ in 0..self.config.max_turns {
            if self.cancel.is_cancelled() {
                return Ok(StopCondition::Cancelled);
            }
            if let Err(exhausted) = self.budget.admit() {
                info!(%exhausted, "model call refused");
                return Ok(StopCondition::BudgetExhausted);
            }
            let call = info_span!(
                PROVIDER_CALL_SPAN,
                { CODA_SESSION_ID } = %self.config.session,
            );
            let mut draft = Draft::default();
            let streamed = self.stream_reply(&mut draft).instrument(call).await;
            // Tokens were spent even when the stream failed or was cancelled.
            if let Some(usage) = draft.usage {
                self.budget.record(usage);
                self.emit(TurnEvent::Usage(usage));
            }
            let reply = match streamed? {
                Streamed::Reply(reply) => reply,
                Streamed::Overflow => {
                    self.emit(TurnEvent::ContextOverflow);
                    return Ok(StopCondition::ContextOverflow);
                }
                Streamed::Cancelled => return Ok(StopCondition::Cancelled),
            };
            // Calls from a reply that did not stop for tools are dropped: nothing
            // would answer them, and an unanswered call poisons every later request.
            let calls = if reply.stop == StopReason::ToolUse {
                reply.calls
            } else {
                Vec::new()
            };
            let text = if reply.text.is_empty() && calls.is_empty() {
                NO_REPLY.to_owned()
            } else {
                reply.text
            };
            self.emit(TurnEvent::Assistant {
                text: text.clone(),
                tool_calls: calls.clone(),
            });
            self.conversation.push(Message::Assistant {
                text,
                tool_calls: calls.clone(),
            });
            match reply.stop {
                StopReason::ToolUse if calls.is_empty() => return Ok(StopCondition::EndTurn),
                StopReason::ToolUse => {
                    if let Some(stop) = self.run_tools(calls).await {
                        return Ok(stop);
                    }
                }
                StopReason::EndTurn => return Ok(StopCondition::EndTurn),
                StopReason::MaxTokens => return Ok(StopCondition::MaxTokens),
                StopReason::Refusal => return Ok(StopCondition::Refused),
                StopReason::Paused => return Ok(StopCondition::Paused),
            }
        }
        Ok(StopCondition::MaxTurns)
    }

    fn emit(&self, event: TurnEvent) {
        // A closed receiver means nobody is watching; the loop carries on.
        let _ = self.events.send(event);
    }

    fn request(&self) -> ModelRequest {
        let prefix = self.conversation.prefix();
        ModelRequest {
            system: prefix.system().to_owned(),
            tools: prefix
                .tools()
                .iter()
                .map(|tool| ModelToolSchema {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                })
                .collect(),
            messages: self
                .conversation
                .messages()
                .iter()
                .map(request_message)
                .collect(),
        }
    }

    /// Streams one model reply, giving up as soon as the user cancels.
    async fn stream_reply(&self, draft: &mut Draft) -> Result<Streamed, TurnError> {
        let opened = tokio::select! {
            opened = self.provider.stream(self.request()) => opened,
            () = self.cancel.cancelled() => return Ok(Streamed::Cancelled),
        };
        let mut stream = match opened {
            Ok(stream) => stream,
            Err(ProviderError::ContextWindowExceeded) => return Ok(Streamed::Overflow),
            Err(error) => return Err(error.into()),
        };
        let stop = loop {
            let item = tokio::select! {
                item = stream.next() => item,
                () = self.cancel.cancelled() => return Ok(Streamed::Cancelled),
            };
            match item {
                None => return Err(TurnError::StreamEnded),
                Some(Err(ProviderError::ContextWindowExceeded)) => return Ok(Streamed::Overflow),
                Some(Err(error)) => return Err(error.into()),
                Some(Ok(event)) => {
                    if let Some(stop) = self.apply(draft, event) {
                        break stop;
                    }
                }
            }
        };
        Ok(Streamed::Reply(Reply {
            text: std::mem::take(&mut draft.text),
            calls: std::mem::take(&mut draft.calls),
            stop,
        }))
    }

    /// Folds one stream event into `reply`; returns the stop reason when the
    /// event ends the stream.
    fn apply(&self, reply: &mut Draft, event: ModelEvent) -> Option<StopReason> {
        match event {
            ModelEvent::TextDelta { text } => {
                reply.text.push_str(&text);
                self.emit(TurnEvent::Text(text));
            }
            ModelEvent::ToolCallStart { call_id, name } => reply.calls.push(ToolCall {
                id: call_id,
                name,
                arguments: String::new(),
            }),
            ModelEvent::ToolCallArgumentsDelta { call_id, fragment } => {
                if let Some(call) = reply.calls.iter_mut().find(|call| call.id == call_id) {
                    call.arguments.push_str(&fragment);
                }
            }
            ModelEvent::Usage(usage) => reply.usage = Some(usage),
            ModelEvent::Stop { reason } => return Some(reason),
        }
        None
    }

    /// Runs the calls in order and records each result. Stops early, still
    /// answering every remaining call, when the user cancels or the budget runs out.
    async fn run_tools(&mut self, calls: Vec<ToolCall>) -> Option<StopCondition> {
        let mut stop = None;
        for call in calls {
            let outcome = if stop.is_some() {
                ToolOutcome::error(SKIPPED.to_owned())
            } else {
                self.run_tool(&call).await
            };
            self.emit(TurnEvent::ToolFinished {
                tool: call.name.clone(),
                call_id: call.id.clone(),
                text: outcome.text.clone(),
                is_error: outcome.is_error,
            });
            self.conversation.push(Message::ToolResult {
                call_id: call.id,
                text: outcome.text,
                is_error: outcome.is_error,
            });
            if stop.is_none() {
                stop = self.after_tool();
            }
        }
        stop
    }

    fn after_tool(&self) -> Option<StopCondition> {
        if self.cancel.is_cancelled() {
            Some(StopCondition::Cancelled)
        } else if self.budget.is_exhausted() {
            Some(StopCondition::BudgetExhausted)
        } else {
            None
        }
    }

    async fn run_tool(&self, call: &ToolCall) -> ToolOutcome {
        let span = info_span!(
            TOOL_CALL_SPAN,
            { CODA_SESSION_ID } = %self.config.session,
            { CODA_TOOL_NAME } = call.name.as_str(),
        );
        self.run_tool_inner(call).instrument(span).await
    }

    async fn run_tool_inner(&self, call: &ToolCall) -> ToolOutcome {
        let Some(tool) = self.tools.tools.get(call.name.as_str()) else {
            return ToolOutcome::error(format!("unknown tool {}", call.name));
        };
        let arguments = if call.arguments.is_empty() {
            EMPTY_ARGUMENTS
        } else {
            &call.arguments
        };
        let input: Value = match serde_json::from_str(arguments) {
            Ok(input) => input,
            Err(error) => {
                return ToolOutcome::error(format!(
                    "the arguments for {} are not valid JSON: {error}",
                    call.name
                ));
            }
        };
        let request = tool_request(&call.name, &input, &self.config.cwd);
        let decision = tokio::select! {
            decision = self.gate.request(&request) => decision,
            () = self.cancel.cancelled() => {
                info!(tool = %call.name, "permission prompt cancelled");
                return ToolOutcome::error(PROMPT_CANCELLED.to_owned());
            }
        };
        if decision == PermissionDecision::Deny {
            info!(tool = %call.name, "tool call denied");
            return ToolOutcome::error(format!("permission to run {} was denied", call.name));
        }
        let work = ToolWork {
            tool: Arc::clone(tool),
            input,
            cancel: self.cancel.flag(),
            store: Arc::clone(&self.store),
            session: self.config.session.clone(),
            threshold: self.config.compact_threshold_bytes,
        };
        match tokio::task::spawn_blocking(move || work.run()).await {
            Ok(outcome) => outcome,
            Err(error) => ToolOutcome::error(format!("the {} tool crashed: {error}", call.name)),
        }
    }
}

/// What one model call produced, before its stop reason arrived.
#[derive(Default)]
struct Draft {
    text: String,
    calls: Vec<ToolCall>,
    usage: Option<Usage>,
}

struct Reply {
    text: String,
    calls: Vec<ToolCall>,
    stop: StopReason,
}

enum Streamed {
    Reply(Reply),
    Overflow,
    Cancelled,
}

struct ToolOutcome {
    text: String,
    is_error: bool,
}

impl ToolOutcome {
    fn error(text: String) -> Self {
        Self {
            text,
            is_error: true,
        }
    }
}

/// A tool call ready to run off the async threads.
struct ToolWork {
    tool: Arc<dyn Tool>,
    input: Value,
    cancel: Arc<AtomicBool>,
    store: Arc<Mutex<Store>>,
    session: SessionId,
    threshold: usize,
}

impl ToolWork {
    fn run(self) -> ToolOutcome {
        match self.tool.call_cancellable(&self.input, &self.cancel) {
            Ok(output) => ToolOutcome {
                text: self.shrink(output.text),
                is_error: false,
            },
            Err(error) => ToolOutcome::error(error.to_string()),
        }
    }

    /// Compacts oversized output. When the store cannot be used the model gets
    /// the output's head and tail plus the reason, never an unbounded blob.
    fn shrink(&self, text: String) -> String {
        if text.len() <= self.threshold {
            return text;
        }
        let Ok(mut store) = self.store.lock() else {
            warn!("the store lock is poisoned, passing a capped output through");
            return capped(&text, "the output store is unavailable");
        };
        match compact(&mut store, &self.session, text.as_bytes()) {
            Ok(compacted) => compacted.text,
            Err(error) => {
                warn!(%error, "compaction failed, passing a capped output through");
                let reason: String = error.to_string().chars().take(MAX_ERROR_CHARS).collect();
                capped(&text, &format!("compaction failed: {reason}"))
            }
        }
    }
}

fn capped(text: &str, reason: &str) -> String {
    format!("{}\n[{reason}]", cap_ends(text))
}

fn tool_request(name: &str, input: &Value, cwd: &Path) -> ToolRequest {
    let tool = match name {
        "bash" => RequestTool::Bash,
        "edit" => RequestTool::Edit,
        "read" => RequestTool::Read,
        "grep" => RequestTool::Grep,
        "retrieve" => RequestTool::Retrieve,
        "code_search" => RequestTool::CodeSearch,
        "code_symbol" => RequestTool::CodeSymbol,
        "code_outline" => RequestTool::CodeOutline,
        "code_read_range" => RequestTool::CodeReadRange,
        other => RequestTool::Unknown(other.to_owned()),
    };
    ToolRequest {
        tool,
        target_paths: input
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .into_iter()
            .collect(),
        command: input
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_owned),
        cwd: cwd.to_owned(),
    }
}

fn request_message(message: &Message) -> RequestMessage {
    match message {
        Message::User(text) => RequestMessage::User(text.clone()),
        Message::Assistant { text, tool_calls } => RequestMessage::Assistant {
            text: text.clone(),
            tool_calls: tool_calls
                .iter()
                .map(|call| RequestToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
        },
        Message::ToolResult {
            call_id,
            text,
            is_error,
        } => RequestMessage::ToolResult {
            call_id: call_id.clone(),
            text: text.clone(),
            is_error: *is_error,
        },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn code_tools_map_to_their_own_permission_tools() {
        // Arrange
        let names = [
            ("code_search", RequestTool::CodeSearch),
            ("code_symbol", RequestTool::CodeSymbol),
            ("code_outline", RequestTool::CodeOutline),
            ("code_read_range", RequestTool::CodeReadRange),
        ];

        for (name, expected) in names {
            // Act
            let request =
                super::tool_request(name, &serde_json::json!({"path": "a.rs"}), Path::new("/ws"));

            // Assert
            assert_eq!(request.tool, expected, "{name}");
        }
    }

    use std::ffi::OsString;
    use std::future::Future;
    use std::path::Path;
    use std::time::Duration;

    use coda_protocol::types::{BackendKind, Mode, PermissionDecision};
    use coda_providers::faux::{FauxProvider, Turn};
    use coda_providers::model::{
        ModelEvent, ModelRequest, ModelStream, RequestMessage, StopReason, Usage,
    };
    use coda_providers::tool_request::ToolRequest;
    use coda_store::NewSession;
    use coda_tools::bash::{BashConfig, BashTool};
    use coda_tools::{ToolError, ToolOutput};
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::sync::mpsc::UnboundedReceiver;

    use super::*;
    use crate::prompt::{Message, Prefix, ToolSchema};

    const GOLDEN: &str = include_str!("../tests/golden/prefix.json");
    const BOUND: Duration = Duration::from_secs(10);
    const POLL: Duration = Duration::from_millis(20);
    const SMALL_USAGE: Usage = Usage {
        input_tokens: 1,
        output_tokens: 1,
    };

    struct Fixed(PermissionDecision);

    impl PermissionGate for Fixed {
        fn request(
            &self,
            _request: &ToolRequest,
        ) -> impl Future<Output = PermissionDecision> + Send {
            std::future::ready(self.0)
        }
    }

    /// A faux provider that remembers every request it was sent.
    struct Recording {
        inner: FauxProvider,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl ModelProvider for Recording {
        fn stream(
            &self,
            request: ModelRequest,
        ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
            self.requests.lock().unwrap().push(request.clone());
            self.inner.stream(request)
        }
    }

    struct Fixture {
        dir: TempDir,
        store: Arc<Mutex<Store>>,
        session: SessionId,
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut store = Store::open(&dir.path().join("data")).unwrap();
            let session = store
                .create_session(&NewSession {
                    backend: BackendKind::ClaudeCode,
                    model: None,
                    cwd: "/ws".to_owned(),
                    mode: Mode::default(),
                })
                .unwrap();
            std::fs::create_dir(dir.path().join("ws")).unwrap();
            Self {
                dir,
                store: Arc::new(Mutex::new(store)),
                session,
                requests: Arc::default(),
            }
        }

        fn workdir(&self) -> PathBuf {
            self.dir.path().join("ws")
        }

        fn config(&self) -> TurnConfig {
            TurnConfig {
                session: self.session.clone(),
                cwd: self.workdir(),
                max_turns: 8,
                token_limit: 1_000_000,
                compact_threshold_bytes: 1024,
            }
        }

        fn turn_loop(
            &self,
            turns: Vec<Turn>,
            gate: PermissionDecision,
            tools: ToolSet,
            config: TurnConfig,
        ) -> (TurnLoop<Recording, Fixed>, UnboundedReceiver<TurnEvent>) {
            let provider = Recording {
                inner: FauxProvider::new(turns),
                requests: Arc::clone(&self.requests),
            };
            self.build(provider, Fixed(gate), tools, config)
        }

        /// A loop that allows every tool call, with the default config.
        fn allowing(
            &self,
            turns: Vec<Turn>,
            tools: ToolSet,
        ) -> (TurnLoop<Recording, Fixed>, UnboundedReceiver<TurnEvent>) {
            self.turn_loop(turns, PermissionDecision::AllowOnce, tools, self.config())
        }

        fn build<P: ModelProvider, G: PermissionGate>(
            &self,
            provider: P,
            gate: G,
            tools: ToolSet,
            config: TurnConfig,
        ) -> (TurnLoop<P, G>, UnboundedReceiver<TurnEvent>) {
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            let prefix = Prefix::new(
                "You are Coda.",
                vec![read_schema(), grep_schema()],
                "Use tabs.",
            )
            .unwrap();
            let lp = TurnLoop::new(
                provider,
                gate,
                tools,
                Arc::clone(&self.store),
                Conversation::new(prefix),
                config,
                sender,
            );
            (lp, receiver)
        }

        fn calls(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn request(&self, index: usize) -> ModelRequest {
            self.requests.lock().unwrap()[index].clone()
        }

        fn bash(&self, timeout: Duration) -> ToolSet {
            let config = BashConfig {
                timeout,
                max_output_bytes: 4096,
                env: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            };
            ToolSet::new().with(Arc::new(BashTool::new(self.workdir(), config)))
        }
    }

    fn read_schema() -> ToolSchema {
        ToolSchema {
            name: "read".into(),
            description: "Read a file.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
        }
    }

    fn grep_schema() -> ToolSchema {
        ToolSchema {
            name: "grep".into(),
            description: "Search files.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"pattern": {"type": "string"}},
                "required": ["pattern"],
            }),
        }
    }

    fn stop_turn(text: &str, reason: StopReason, usage: Usage) -> Turn {
        Turn {
            events: vec![
                ModelEvent::TextDelta { text: text.into() },
                ModelEvent::Usage(usage),
                ModelEvent::Stop { reason },
            ],
            error: None,
        }
    }

    fn end_turn(text: &str) -> Turn {
        stop_turn(text, StopReason::EndTurn, SMALL_USAGE)
    }

    fn tool_turn(calls: &[(&str, &str, &str)], usage: Usage) -> Turn {
        let mut events = Vec::new();
        for (id, name, arguments) in calls {
            events.push(ModelEvent::ToolCallStart {
                call_id: (*id).into(),
                name: (*name).into(),
            });
            // Split the arguments in two so reassembly is exercised.
            let (head, tail) = arguments.split_at(arguments.len() / 2);
            for fragment in [head, tail] {
                events.push(ModelEvent::ToolCallArgumentsDelta {
                    call_id: (*id).into(),
                    fragment: fragment.into(),
                });
            }
        }
        events.push(ModelEvent::Usage(usage));
        events.push(ModelEvent::Stop {
            reason: StopReason::ToolUse,
        });
        Turn {
            events,
            error: None,
        }
    }

    fn tool_result(request: &ModelRequest) -> (String, bool) {
        match request.messages.last() {
            Some(RequestMessage::ToolResult { text, is_error, .. }) => (text.clone(), *is_error),
            other => panic!("expected a tool result last, got {other:?}"),
        }
    }

    /// A tool that returns fixed text, or fails when it has none.
    struct Canned(Option<String>);

    impl Tool for Canned {
        fn name(&self) -> &'static str {
            "canned"
        }

        fn call(&self, _input: &Value) -> Result<ToolOutput, ToolError> {
            self.0
                .clone()
                .map(|text| ToolOutput {
                    text,
                    truncated: false,
                })
                .ok_or(ToolError::InvalidInput {
                    tool: "canned",
                    reason: "bad field".into(),
                })
        }
    }

    fn canned(text: Option<&str>) -> ToolSet {
        ToolSet::new().with(Arc::new(Canned(text.map(str::to_owned))))
    }

    async fn poll_until(mut done: impl FnMut() -> bool) {
        tokio::time::timeout(BOUND, async {
            while !done() {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("condition not reached in time");
    }

    fn process_alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    async fn read_pid(path: &Path) -> String {
        poll_until(|| std::fs::read_to_string(path).is_ok_and(|s| s.ends_with('\n'))).await;
        std::fs::read_to_string(path).unwrap().trim().to_owned()
    }

    #[tokio::test]
    async fn every_stop_reason_is_reported() {
        // Arrange
        use StopCondition as S;
        let looping = || tool_turn(&[("c", "missing", "{}")], SMALL_USAGE);
        let ends = |reason| stop_turn("..", reason, SMALL_USAGE);
        let overflow = Turn {
            events: Vec::new(),
            error: Some(ProviderError::ContextWindowExceeded),
        };
        let cases = [
            ("end turn", vec![end_turn("hi")], 8, S::EndTurn, 1),
            (
                "max turns",
                vec![looping(), looping(), looping()],
                2,
                S::MaxTurns,
                2,
            ),
            ("context overflow", vec![overflow], 8, S::ContextOverflow, 1),
            ("refusal", vec![ends(StopReason::Refusal)], 8, S::Refused, 1),
            ("paused", vec![ends(StopReason::Paused)], 8, S::Paused, 1),
            (
                "output limit",
                vec![ends(StopReason::MaxTokens)],
                8,
                S::MaxTokens,
                1,
            ),
        ];

        for (name, turns, max_turns, expected, expected_calls) in cases {
            let fixture = Fixture::new();
            let mut config = fixture.config();
            config.max_turns = max_turns;
            let (mut lp, mut events) =
                fixture.turn_loop(turns, PermissionDecision::AllowOnce, ToolSet::new(), config);

            // Act
            let stop = lp.send("go").await.unwrap();

            // Assert
            assert_eq!(stop, expected, "{name}");
            assert_eq!(fixture.calls(), expected_calls, "{name}");
            let announced = events.try_recv() == Ok(TurnEvent::ContextOverflow);
            assert_eq!(announced, expected == S::ContextOverflow, "{name}");
        }
    }

    #[tokio::test]
    async fn a_limit_reached_mid_tool_loop_stops_after_the_current_call() {
        // Arrange
        let fixture = Fixture::new();
        let mut config = fixture.config();
        config.token_limit = 10;
        let at_limit = Usage {
            input_tokens: 6,
            output_tokens: 4,
        };
        let two_calls = tool_turn(&[("c1", "canned", "{}"), ("c2", "canned", "{}")], at_limit);
        let (mut lp, _events) = fixture.turn_loop(
            vec![two_calls, end_turn("never")],
            PermissionDecision::AllowOnce,
            canned(Some("ran")),
            config,
        );

        // Act
        let stop = lp.send("go").await.unwrap();

        // Assert
        assert_eq!(stop, StopCondition::BudgetExhausted);
        assert_eq!(fixture.calls(), 1);
        let results: Vec<_> = lp
            .conversation()
            .messages()
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult {
                    call_id,
                    text,
                    is_error,
                } => Some((call_id.as_str(), text.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(results[0], ("c1", "ran", false));
        assert_eq!(results[1].0, "c2");
        assert!(results[1].2, "the skipped call is reported as an error");
    }

    #[tokio::test]
    async fn a_denied_tool_call_is_fed_back_and_not_run() {
        // Arrange
        let fixture = Fixture::new();
        let marker = fixture.workdir().join("ran");
        let command = json!({"command": "touch ran"}).to_string();
        let (mut lp, _events) = fixture.turn_loop(
            vec![
                tool_turn(&[("c1", "bash", &command)], SMALL_USAGE),
                end_turn("ok"),
            ],
            PermissionDecision::Deny,
            fixture.bash(BOUND),
            fixture.config(),
        );

        // Act
        let stop = lp.send("go").await.unwrap();

        // Assert
        assert_eq!(stop, StopCondition::EndTurn);
        let (text, is_error) = tool_result(&fixture.request(1));
        assert!(is_error);
        assert!(text.contains("denied"), "{text}");
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn each_tool_outcome_is_fed_back_and_the_loop_continues() {
        // Arrange
        let cases = [
            ("tool error", canned(None), "{}", "bad field", true),
            (
                "bad json",
                canned(Some("x")),
                "{not json",
                "not valid JSON",
                true,
            ),
            (
                "success",
                canned(Some("small output")),
                "{}",
                "small output",
                false,
            ),
        ];

        for (name, tools, arguments, expected, expected_error) in cases {
            let fixture = Fixture::new();
            let (mut lp, _events) = fixture.allowing(
                vec![
                    tool_turn(&[("c1", "canned", arguments)], SMALL_USAGE),
                    end_turn("ok"),
                ],
                tools,
            );

            // Act
            let stop = lp.send("go").await.unwrap();

            // Assert
            assert_eq!(stop, StopCondition::EndTurn, "{name}");
            let (text, is_error) = tool_result(&fixture.request(1));
            assert_eq!(is_error, expected_error, "{name}");
            assert!(text.contains(expected), "{name}: {text}");
        }
    }

    #[tokio::test]
    async fn an_oversized_result_is_compacted_with_a_retrieve_id_and_counted() {
        // Arrange
        let fixture = Fixture::new();
        let original: String = "a line of the log\n".repeat(400);
        let (mut lp, _events) = fixture.allowing(
            vec![
                tool_turn(&[("c1", "canned", "{}")], SMALL_USAGE),
                end_turn("ok"),
            ],
            canned(Some(&original)),
        );

        // Act
        lp.send("go").await.unwrap();

        // Assert
        let (text, is_error) = tool_result(&fixture.request(1));
        assert!(!is_error);
        assert!(text.len() < original.len());
        let id = text
            .rsplit("retrieve id ")
            .next()
            .unwrap()
            .trim_end_matches(']');
        let store = fixture.store.lock().unwrap();
        let stored = store
            .load_original(&fixture.session, &coda_protocol::types::OriginalId::new(id))
            .unwrap();
        assert_eq!(stored.as_deref(), Some(original.as_str()));
        let saved = store.saved_bytes(Some(&fixture.session)).unwrap();
        assert_eq!(saved, (original.len() - text.len()) as u64);
    }

    #[tokio::test]
    async fn the_prefix_is_identical_across_turns_and_earlier_messages_never_change() {
        // Arrange
        let fixture = Fixture::new();
        let (mut lp, _events) = fixture.allowing(
            vec![
                tool_turn(&[("c1", "canned", "{}")], SMALL_USAGE),
                end_turn("first answer"),
                end_turn("second answer"),
            ],
            canned(Some("result mentioning /tmp/x at noon")),
        );

        // Act
        lp.send("first").await.unwrap();
        let after_first = lp.conversation().messages().to_vec();
        lp.send("second").await.unwrap();

        // Assert
        assert_eq!(lp.conversation().prefix().bytes(), GOLDEN.as_bytes());
        let (first, last) = (fixture.request(0), fixture.request(2));
        assert_eq!(first.system, last.system);
        assert_eq!(first.tools, last.tools);
        assert_eq!(first.tools.len(), 2);
        assert_eq!(
            &lp.conversation().messages()[..after_first.len()],
            after_first.as_slice()
        );
        assert_eq!(last.messages.len(), after_first.len() + 1);
    }

    #[tokio::test]
    async fn cancel_during_a_tool_call_kills_the_command_and_its_grandchild() {
        // Arrange
        let fixture = Fixture::new();
        let command = json!({"command": "sleep 300 & echo $! > pid; wait"}).to_string();
        let (mut lp, _events) = fixture.allowing(
            vec![tool_turn(&[("c1", "bash", &command)], SMALL_USAGE)],
            fixture.bash(BOUND),
        );
        let cancel = lp.cancel_token();
        let task = tokio::spawn(async move { lp.send("go").await });
        let grandchild = read_pid(&fixture.workdir().join("pid")).await;
        assert!(process_alive(&grandchild));

        // Act
        cancel.cancel();
        let stop = tokio::time::timeout(BOUND, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        // Assert
        assert_eq!(stop, StopCondition::Cancelled);
        poll_until(|| !process_alive(&grandchild)).await;
    }

    /// Streams one text delta and then stalls on its first call; later calls
    /// answer from `inner`.
    struct Stalled {
        inner: FauxProvider,
        used: AtomicBool,
    }

    impl ModelProvider for Stalled {
        fn stream(
            &self,
            request: ModelRequest,
        ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
            let first = !self.used.swap(true, Ordering::SeqCst);
            async move {
                if !first {
                    return self.inner.stream(request).await;
                }
                Ok(ModelStream::spawn(4, |sender| async move {
                    let _ = sender
                        .send(Ok(ModelEvent::TextDelta { text: "par".into() }))
                        .await;
                    std::future::pending::<()>().await;
                }))
            }
        }
    }

    /// A gate whose prompt is never answered; records when it was dropped.
    struct Unanswered {
        asked: Arc<AtomicBool>,
        dropped: Arc<AtomicBool>,
    }

    struct SetOnDrop(Arc<AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl PermissionGate for Unanswered {
        fn request(
            &self,
            _request: &ToolRequest,
        ) -> impl Future<Output = PermissionDecision> + Send {
            let guard = SetOnDrop(Arc::clone(&self.dropped));
            self.asked.store(true, Ordering::SeqCst);
            async move {
                let _guard = guard;
                std::future::pending().await
            }
        }
    }

    /// Asserts a user message starts every exchange, every call has exactly
    /// one result straight after it, and the history ends on the assistant.
    fn assert_valid_history(messages: &[Message]) {
        let mut open = false;
        let mut index = 0;
        while index < messages.len() {
            match &messages[index] {
                Message::User(_) => {
                    assert!(!open, "unexpected user message at {index}");
                    open = true;
                }
                Message::Assistant { tool_calls, .. } => {
                    assert!(open, "unexpected assistant message at {index}");
                    for call in tool_calls {
                        index += 1;
                        let found = messages.get(index);
                        assert!(
                            matches!(found, Some(Message::ToolResult { call_id, .. }) if *call_id == call.id),
                            "call {} unanswered, got {found:?}",
                            call.id
                        );
                    }
                    open = !tool_calls.is_empty();
                }
                Message::ToolResult { .. } => panic!("stray tool result at {index}"),
            }
            index += 1;
        }
        assert!(!open, "history must end on a plain assistant message");
    }

    fn with_stop(mut turn: Turn, reason: StopReason) -> Turn {
        *turn.events.last_mut().unwrap() = ModelEvent::Stop { reason };
        turn
    }

    #[tokio::test]
    async fn calls_in_a_reply_that_did_not_stop_for_tools_are_not_stored() {
        for reason in [
            StopReason::MaxTokens,
            StopReason::Refusal,
            StopReason::Paused,
            StopReason::EndTurn,
        ] {
            // Arrange
            let fixture = Fixture::new();
            let cut_off = with_stop(tool_turn(&[("c1", "canned", "{}")], SMALL_USAGE), reason);
            let (mut lp, _events) =
                fixture.allowing(vec![cut_off, end_turn("fine")], canned(Some("ran")));

            // Act
            lp.send("first").await.unwrap();
            let next = lp.send("second").await.unwrap();

            // Assert
            assert_eq!(next, StopCondition::EndTurn, "{reason:?}");
            assert_valid_history(lp.conversation().messages());
            let stored_calls = lp.conversation().messages().iter().any(
                |m| matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()),
            );
            assert!(!stored_calls, "{reason:?}");
        }
    }

    #[tokio::test]
    async fn a_tool_use_stop_without_calls_ends_the_turn() {
        // Arrange
        let fixture = Fixture::new();
        let empty = Turn {
            events: vec![ModelEvent::Stop {
                reason: StopReason::ToolUse,
            }],
            error: None,
        };
        let (mut lp, _events) = fixture.allowing(vec![empty, end_turn("again")], ToolSet::new());

        // Act
        let stop = lp.send("go").await.unwrap();
        lp.send("more").await.unwrap();

        // Assert
        assert_eq!(stop, StopCondition::EndTurn);
        assert_eq!(fixture.calls(), 2);
        assert_valid_history(lp.conversation().messages());
    }

    #[tokio::test]
    async fn cancel_during_streaming_stops_the_send_and_the_next_send_works() {
        // Arrange
        let fixture = Fixture::new();
        let (mut lp, mut events) = fixture.build(
            Stalled {
                inner: FauxProvider::new(vec![end_turn("back")]),
                used: AtomicBool::new(false),
            },
            Fixed(PermissionDecision::AllowOnce),
            ToolSet::new(),
            fixture.config(),
        );
        let cancel = lp.cancel_token();
        let task = tokio::spawn(async move {
            let stop = lp.send("go").await;
            (lp, stop)
        });
        let first = tokio::time::timeout(BOUND, events.recv()).await.unwrap();
        assert_eq!(first, Some(TurnEvent::Text("par".into())));

        // Act
        cancel.cancel();
        let (mut lp, stop) = tokio::time::timeout(BOUND, task).await.unwrap().unwrap();
        cancel.reset();
        let next = lp.send("again").await.unwrap();

        // Assert
        assert_eq!(stop.unwrap(), StopCondition::Cancelled);
        assert_eq!(next, StopCondition::EndTurn);
        assert_valid_history(lp.conversation().messages());
    }

    #[tokio::test]
    async fn cancel_while_a_permission_prompt_is_pending_skips_the_tool() {
        // Arrange
        let fixture = Fixture::new();
        let asked = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let command = json!({"command": "touch ran"}).to_string();
        let (mut lp, _events) = fixture.build(
            Recording {
                inner: FauxProvider::new(vec![
                    tool_turn(&[("c1", "bash", &command)], SMALL_USAGE),
                    end_turn("ok"),
                ]),
                requests: Arc::clone(&fixture.requests),
            },
            Unanswered {
                asked: Arc::clone(&asked),
                dropped: Arc::clone(&dropped),
            },
            fixture.bash(BOUND),
            fixture.config(),
        );
        let cancel = lp.cancel_token();
        let task = tokio::spawn(async move {
            let stop = lp.send("go").await;
            (lp, stop)
        });
        poll_until(|| asked.load(Ordering::SeqCst)).await;

        // Act
        cancel.cancel();
        let (lp, stop) = tokio::time::timeout(BOUND, task).await.unwrap().unwrap();

        // Assert
        assert_eq!(stop.unwrap(), StopCondition::Cancelled);
        assert!(
            dropped.load(Ordering::SeqCst),
            "the pending prompt is dropped"
        );
        assert!(!fixture.workdir().join("ran").exists());
        assert_valid_history(lp.conversation().messages());
        let answered = lp.conversation().messages().iter().any(|m| {
            matches!(m, Message::ToolResult { text, is_error: true, .. } if text.contains("cancelled"))
        });
        assert!(answered);
    }

    #[tokio::test]
    async fn a_failed_send_leaves_a_history_the_next_send_can_extend() {
        // Arrange
        let fixture = Fixture::new();
        let (mut lp, _events) = fixture.allowing(
            vec![Turn {
                events: Vec::new(),
                error: Some(ProviderError::Fatal {
                    message: "down".into(),
                }),
            }],
            ToolSet::new(),
        );

        // Act
        let failed = lp.send("one").await;
        let second = lp.send("two").await;

        // Assert
        assert!(failed.is_err());
        assert!(second.is_err());
        assert_valid_history(lp.conversation().messages());
    }

    #[tokio::test]
    async fn an_unusable_store_gives_the_model_a_capped_output_and_the_reason() {
        for (name, poison, suffix) in [
            ("missing session", false, "[compaction failed:"),
            ("poisoned lock", true, "[the output store is unavailable]"),
        ] {
            // Arrange
            let fixture = Fixture::new();
            let mut config = fixture.config();
            if poison {
                let store = Arc::clone(&fixture.store);
                let _ = std::thread::spawn(move || {
                    let _held = store.lock().unwrap();
                    panic!("a tool died holding the store");
                })
                .join();
            } else {
                config.session = SessionId::new("no-such-session");
            }
            let original = "a line of the log\n".repeat(2000);
            let (mut lp, _events) = fixture.turn_loop(
                vec![
                    tool_turn(&[("c1", "canned", "{}")], SMALL_USAGE),
                    end_turn("ok"),
                ],
                PermissionDecision::AllowOnce,
                canned(Some(&original)),
                config,
            );

            // Act
            lp.send("go").await.unwrap();

            // Assert
            let (text, is_error) = tool_result(&fixture.request(1));
            assert!(!is_error, "{name}");
            assert!(text.len() < original.len() / 2, "{name}: {}", text.len());
            assert!(text.contains(suffix), "{name}: {text}");
        }
    }

    #[tokio::test]
    async fn each_model_call_reports_its_usage() {
        // Arrange
        let fixture = Fixture::new();
        let first = Usage {
            input_tokens: 11,
            output_tokens: 7,
        };
        let (mut lp, mut events) = fixture.allowing(
            vec![tool_turn(&[("c1", "canned", "{}")], first), end_turn("ok")],
            canned(Some("ran")),
        );

        // Act
        lp.send("go").await.unwrap();

        // Assert
        let mut usages = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let TurnEvent::Usage(usage) = event {
                usages.push(usage);
            }
        }
        assert_eq!(usages, vec![first, SMALL_USAGE]);
    }

    #[tokio::test]
    async fn a_cancel_that_arrived_before_the_send_polls_stops_it() {
        // Arrange
        let fixture = Fixture::new();
        let (mut lp, _events) = fixture.allowing(vec![end_turn("never")], canned(None));
        lp.cancel_token().cancel();

        // Act
        let stop = lp.send("go").await.unwrap();

        // Assert
        assert_eq!(stop, StopCondition::Cancelled);
        assert_eq!(fixture.calls(), 0);
    }

    #[tokio::test]
    async fn usage_reported_before_a_stream_failure_is_still_emitted() {
        // Arrange
        let fixture = Fixture::new();
        let partial = Turn {
            events: vec![ModelEvent::Usage(SMALL_USAGE)],
            error: Some(ProviderError::Fatal {
                message: "dropped".into(),
            }),
        };
        let (mut lp, mut events) = fixture.allowing(vec![partial], canned(None));

        // Act
        let result = lp.send("go").await;

        // Assert
        assert!(result.is_err());
        assert_eq!(events.try_recv(), Ok(TurnEvent::Usage(SMALL_USAGE)));
    }
}
