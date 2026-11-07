// SPDX-License-Identifier: Apache-2.0

//! Runs one fixture through the turn loop and judges the result.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coda_config::hooks::{HookConfig, HookEvent};
use coda_core::hooks::HookRunner;
use coda_core::prompt::{Conversation, Prefix};
use coda_core::tools::{BaseToolsConfig, StoreOriginals, base_tools};
use coda_core::turn::{StopCondition, TurnConfig, TurnEvent, TurnLoop};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision};
use coda_providers::faux::FauxProvider;
use coda_providers::gate::PermissionGate;
use coda_providers::model::{
    ModelProvider, ModelRequest, ModelStream, ProviderError, RequestMessage,
};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::fixtures::Fixture;

const SYSTEM_PROMPT: &str = "You are Coda, a coding agent. Use the tools to finish the task, then reply with a short summary.";
const MAX_TURNS: u32 = 12;
const TOKEN_LIMIT: u64 = 10_000_000;
/// Tool output above this many bytes is shrunk, low enough that a noisy test run is.
const COMPACT_THRESHOLD_BYTES: usize = 2048;
const BASH_TIMEOUT: Duration = Duration::from_secs(20);
const GUARD_TIMEOUT_MS: u64 = 10_000;
const SHELL_PATH: &str = "/usr/bin:/bin";
/// A rough size of a token in request bytes, enough to compare runs with each other.
const BYTES_PER_TOKEN: u64 = 4;

/// What one fixture run produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub name: &'static str,
    pub passed: bool,
    pub turns: u32,
    /// Estimated tokens of every request sent, so a longer tool text or an extra
    /// call shows up even where the model's own usage is scripted.
    pub tokens: u64,
    /// Tokens the provider reported; only a real model makes this meaningful.
    pub usage_tokens: u64,
    pub shrunk_bytes: u64,
    pub tool_errors: u32,
    pub stop: String,
    pub failure: Option<String>,
}

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

/// A provider that remembers every request it was sent.
struct Recording<P> {
    inner: P,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl<P: ModelProvider + Sync> ModelProvider for Recording<P> {
    fn stream(
        &self,
        request: ModelRequest,
    ) -> impl Future<Output = Result<ModelStream, ProviderError>> + Send {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.stream(request)
    }
}

fn request_bytes(request: &ModelRequest) -> u64 {
    let tools: usize = request
        .tools
        .iter()
        .map(|tool| tool.name.len() + tool.description.len() + tool.input_schema.to_string().len())
        .sum();
    let messages: usize = request
        .messages
        .iter()
        .map(|message| match message {
            RequestMessage::User(text) | RequestMessage::ToolResult { text, .. } => text.len(),
            RequestMessage::Assistant { text, tool_calls } => {
                text.len()
                    + tool_calls
                        .iter()
                        .map(|call| call.name.len() + call.arguments.len())
                        .sum::<usize>()
            }
        })
        .sum();
    (request.system.len() + tools + messages) as u64
}

pub async fn run_offline(fixture: &Fixture) -> Outcome {
    let provider = FauxProvider::new((fixture.script)());
    run(fixture, provider).await
}

/// Runs `fixture` against `provider` in a fresh workspace.
pub async fn run<P: ModelProvider + Send + Sync>(fixture: &Fixture, provider: P) -> Outcome {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path().join("ws");
    std::fs::create_dir(&workdir).unwrap();
    for (path, content) in fixture.files {
        std::fs::write(workdir.join(path), content).unwrap();
    }
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let session = store
        .create_session(&NewSession {
            backend: BackendKind::Faux,
            model: None,
            cwd: workdir.display().to_string(),
            mode: Mode::default(),
        })
        .unwrap();
    let store = Arc::new(Mutex::new(store));
    let tools = base_tools(
        &BaseToolsConfig {
            workdir: workdir.clone(),
            bash_timeout: BASH_TIMEOUT,
            env: vec![(OsString::from("PATH"), OsString::from(SHELL_PATH))],
        },
        Arc::clone(&store),
        StoreOriginals::new(Arc::clone(&store), session.clone()),
    )
    .unwrap();
    let prefix = Prefix::new(SYSTEM_PROMPT, tools.schemas, "").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut lp = TurnLoop::new(
        Recording {
            inner: provider,
            requests: Arc::clone(&requests),
        },
        AllowAll,
        tools.tools,
        Arc::clone(&store),
        Conversation::new(prefix),
        TurnConfig {
            session: session.clone(),
            cwd: workdir.clone(),
            max_turns: MAX_TURNS,
            token_limit: TOKEN_LIMIT,
            compact_threshold_bytes: COMPACT_THRESHOLD_BYTES,
        },
        sender,
    )
    .with_hooks(guard_hooks(fixture))
    .with_text_tool_calls(fixture.text_calls);

    let sent = lp.send(fixture.prompt).await;
    let (turns, usage_tokens, tool_errors) = count_events(&mut receiver);
    let requests = requests.lock().unwrap().clone();
    let shrunk_bytes = store
        .lock()
        .unwrap()
        .saved_bytes(Some(&session))
        .unwrap_or_default();

    let (stop, mut failure) = match sent {
        Ok(StopCondition::EndTurn) => ("end_turn".to_owned(), None),
        Ok(other) => (
            other.as_str().to_owned(),
            Some(format!("the loop stopped with {}", other.as_str())),
        ),
        Err(error) => (
            "error".to_owned(),
            Some(format!("the loop failed: {error}")),
        ),
    };
    if failure.is_none() {
        failure = judge(fixture, &workdir, &requests);
    }
    Outcome {
        name: fixture.name,
        passed: failure.is_none(),
        turns,
        tokens: requests.iter().map(request_bytes).sum::<u64>() / BYTES_PER_TOKEN,
        usage_tokens,
        shrunk_bytes,
        tool_errors,
        stop,
        failure,
    }
}

/// The fixture's `end_turn` guard, if it has one, as the hook runner of the loop.
fn guard_hooks(fixture: &Fixture) -> HookRunner {
    let hooks: Vec<HookConfig> = fixture
        .guard
        .iter()
        .map(|script| HookConfig {
            event: HookEvent::EndTurn,
            command: ["/bin/sh", "-c", script].map(str::to_owned).to_vec(),
            blocking: true,
            timeout_ms: Some(GUARD_TIMEOUT_MS),
        })
        .collect();
    HookRunner::new(
        &hooks,
        vec![(OsString::from("PATH"), OsString::from(SHELL_PATH))],
    )
}

fn count_events(receiver: &mut UnboundedReceiver<TurnEvent>) -> (u32, u64, u32) {
    let (mut turns, mut tokens, mut errors) = (0, 0, 0);
    while let Ok(event) = receiver.try_recv() {
        match event {
            TurnEvent::Assistant { .. } => turns += 1,
            TurnEvent::Usage(usage) => tokens += usage.input_tokens + usage.output_tokens,
            TurnEvent::ToolFinished { is_error: true, .. } => errors += 1,
            _ => {}
        }
    }
    (turns, tokens, errors)
}

/// Why the run does not count as a pass, if it does not.
fn judge(fixture: &Fixture, workdir: &Path, requests: &[ModelRequest]) -> Option<String> {
    if let Some(marker) = fixture.untrusted {
        let seen = requests.iter().any(|request| {
            request.messages.iter().any(
                |message| matches!(message, RequestMessage::ToolResult { text, .. } if text.contains(marker)),
            )
        });
        if !seen {
            return Some("the untrusted text never reached the model as a tool result".to_owned());
        }
        let leaked = requests.iter().any(|request| {
            request.system.contains(marker)
                || request.messages.iter().any(|message| {
                    matches!(message, RequestMessage::User(text) if text.contains(marker))
                })
        });
        if leaked {
            return Some("the untrusted text appeared outside a tool result".to_owned());
        }
    }
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(fixture.check)
        .current_dir(workdir)
        .env_clear()
        .env("PATH", SHELL_PATH)
        .status();
    match status {
        Ok(status) if status.success() => None,
        Ok(status) => Some(format!("the check command exited with {status}")),
        Err(error) => Some(format!("the check command did not start: {error}")),
    }
}

/// A table of the outcomes, one line per fixture.
pub fn report(outcomes: &[Outcome]) -> String {
    let mut text = String::from("fixture                  result  turns  tokens  usage  shrunk\n");
    for o in outcomes {
        let result = if o.passed { "pass" } else { "FAIL" };
        writeln!(
            text,
            "{:<24} {:<7} {:>5} {:>7} {:>6} {:>7}",
            o.name, result, o.turns, o.tokens, o.usage_tokens, o.shrunk_bytes
        )
        .unwrap();
        if let Some(why) = &o.failure {
            writeln!(text, "  {why}").unwrap();
        }
    }
    text
}
