// SPDX-License-Identifier: Apache-2.0

//! The model fallback chain: a send moves to the next alias when a model fails
//! before it says anything, and never in the middle of a reply.

use std::future::Future;
use std::sync::{Arc, Mutex};

use coda_core::prompt::{Conversation, Prefix, ToolSchema};
use coda_core::turn::{
    Fallback, StopCondition, ToolSet, TurnConfig, TurnError, TurnEvent, TurnLoop,
};
use coda_protocol::types::{BackendKind, Mode, PermissionDecision};
use coda_providers::faux::{FauxProvider, Turn};
use coda_providers::gate::PermissionGate;
use coda_providers::model::{
    ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError, StopReason, Usage,
};
use coda_providers::tool_request::ToolRequest;
use coda_store::{NewSession, Store};
use coda_tools::{Tool, ToolError, ToolOutput};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;

const TOOL: &str = "note";
const PRIMARY: &str = "fast";
const SECOND: &str = "steady";
const THIRD: &str = "spare";
const USED: Usage = Usage {
    input_tokens: 10,
    output_tokens: 5,
};
const TOKENS_PER_REPLY: u64 = 15;

struct AllowAll;

impl PermissionGate for AllowAll {
    fn request(&self, _request: &ToolRequest) -> impl Future<Output = PermissionDecision> + Send {
        std::future::ready(PermissionDecision::AllowOnce)
    }
}

struct Note;

impl Tool for Note {
    fn name(&self) -> &'static str {
        TOOL
    }

    fn call(&self, _input: &Value) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            text: "noted".to_owned(),
            truncated: false,
        })
    }
}

/// A provider that keeps the requests it is sent, then answers from a script.
struct Recording {
    inner: FauxProvider,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Recording {
    fn new(turns: Vec<Turn>) -> (Self, Arc<Mutex<Vec<ModelRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Self {
            inner: FauxProvider::new(turns),
            requests: Arc::clone(&requests),
        };
        (provider, requests)
    }
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

fn failing(error: ProviderError) -> Turn {
    Turn {
        events: Vec::new(),
        error: Some(error),
    }
}

fn rate_limited() -> Turn {
    failing(ProviderError::Retryable {
        message: "HTTP 429".to_owned(),
    })
}

fn answer(text: &str) -> Turn {
    Turn {
        events: vec![
            ModelEvent::TextDelta { text: text.into() },
            ModelEvent::Usage(USED),
            ModelEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ],
        error: None,
    }
}

fn note_call() -> Turn {
    Turn {
        events: vec![
            ModelEvent::ToolCallStart {
                call_id: "c1".into(),
                name: TOOL.into(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                call_id: "c1".into(),
                fragment: "{}".into(),
            },
            ModelEvent::Usage(USED),
            ModelEvent::Stop {
                reason: StopReason::ToolUse,
            },
        ],
        error: None,
    }
}

type Loop = TurnLoop<Recording, AllowAll>;
type Seen = Arc<Mutex<Vec<ModelRequest>>>;

struct Rig {
    lp: Loop,
    events: UnboundedReceiver<TurnEvent>,
    primary: Seen,
    fallbacks: Vec<Seen>,
    _dir: tempfile::TempDir,
}

fn rig(primary: Vec<Turn>, fallbacks: Vec<(&str, Vec<Turn>)>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let session = store
        .create_session(&NewSession {
            backend: BackendKind::Faux,
            model: None,
            cwd: "/ws".to_owned(),
            mode: Mode::default(),
        })
        .unwrap();
    std::fs::create_dir(dir.path().join("ws")).unwrap();
    let schema = ToolSchema {
        name: TOOL.into(),
        description: "Notes something.".into(),
        input_schema: json!({"type": "object", "properties": {}}),
    };
    let prefix = Prefix::new("You are Coda.", vec![schema], "").unwrap();
    let (sender, events) = tokio::sync::mpsc::unbounded_channel();
    let (provider, primary_seen) = Recording::new(primary);
    let mut seen = Vec::new();
    let mut chain = Vec::new();
    for (alias, turns) in fallbacks {
        let (provider, requests) = Recording::new(turns);
        seen.push(requests);
        chain.push(Fallback {
            alias: alias.to_owned(),
            provider,
            context_window: None,
        });
    }
    let lp = TurnLoop::new(
        provider,
        AllowAll,
        ToolSet::new().with(Arc::new(Note)),
        Arc::new(Mutex::new(store)),
        Conversation::new(prefix),
        TurnConfig {
            session,
            cwd: dir.path().join("ws"),
            max_turns: 8,
            token_limit: 1_000_000,
            compact_threshold_bytes: 1024 * 1024,
        },
        sender,
    )
    .with_fallbacks(PRIMARY, chain);
    Rig {
        lp,
        events,
        primary: primary_seen,
        fallbacks: seen,
        _dir: dir,
    }
}

fn switches(events: &mut UnboundedReceiver<TurnEvent>) -> Vec<(String, String, String)> {
    let mut found = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let TurnEvent::ModelFallback { from, to, reason } = event {
            found.push((from, to, reason));
        }
    }
    found
}

fn calls(seen: &Seen) -> usize {
    seen.lock().unwrap().len()
}

#[tokio::test]
async fn a_rate_limited_primary_hands_the_turn_to_the_fallback_once() {
    let mut rig = rig(vec![rate_limited()], vec![(SECOND, vec![answer("done")])]);

    let stop = rig.lp.send("hi").await.unwrap();

    assert_eq!(stop, StopCondition::EndTurn);
    assert_eq!(calls(&rig.primary), 1);
    assert_eq!(calls(&rig.fallbacks[0]), 1);
    let switched = switches(&mut rig.events);
    assert_eq!(switched.len(), 1, "{switched:?}");
    assert_eq!(switched[0].0, PRIMARY);
    assert_eq!(switched[0].1, SECOND);
    assert!(switched[0].2.contains("429"), "{switched:?}");
}

#[tokio::test]
async fn the_fallback_is_sent_the_same_request() {
    let mut rig = rig(vec![rate_limited()], vec![(SECOND, vec![answer("done")])]);

    rig.lp.send("hi").await.unwrap();

    let first = rig.primary.lock().unwrap()[0].clone();
    let second = rig.fallbacks[0].lock().unwrap()[0].clone();
    assert_eq!(first, second);
}

#[tokio::test]
async fn an_unknown_model_falls_back_and_a_fatal_error_does_not() {
    let mut unknown = rig(
        vec![failing(ProviderError::UnknownModel {
            message: "no such model".to_owned(),
        })],
        vec![(SECOND, vec![answer("done")])],
    );
    let mut fatal = rig(
        vec![failing(ProviderError::Fatal {
            message: "bad key".to_owned(),
        })],
        vec![(SECOND, vec![answer("done")])],
    );

    let recovered = unknown.lp.send("hi").await;
    let refused = fatal.lp.send("hi").await;

    assert_eq!(recovered.unwrap(), StopCondition::EndTurn);
    assert!(matches!(
        refused,
        Err(TurnError::Provider(ProviderError::Fatal { .. }))
    ));
    assert_eq!(calls(&fatal.fallbacks[0]), 0);
    assert_eq!(switches(&mut fatal.events).len(), 0);
}

#[tokio::test]
async fn a_failure_after_the_first_byte_never_falls_back() {
    let broken_reply = Turn {
        events: vec![ModelEvent::TextDelta {
            text: "Half an ans".into(),
        }],
        error: Some(ProviderError::Retryable {
            message: "HTTP 529".to_owned(),
        }),
    };
    let mut rig = rig(vec![broken_reply], vec![(SECOND, vec![answer("done")])]);

    let sent = rig.lp.send("hi").await;

    assert!(matches!(
        sent,
        Err(TurnError::Provider(ProviderError::Retryable { .. }))
    ));
    assert_eq!(calls(&rig.fallbacks[0]), 0);
    assert_eq!(switches(&mut rig.events).len(), 0);
}

#[tokio::test]
async fn the_chain_is_walked_in_order_and_the_last_failure_is_reported() {
    let mut walked = rig(
        vec![rate_limited()],
        vec![(SECOND, vec![rate_limited()]), (THIRD, vec![answer("ok")])],
    );
    let mut spent = rig(
        vec![rate_limited()],
        vec![
            (SECOND, vec![rate_limited()]),
            (THIRD, vec![rate_limited()]),
        ],
    );

    let finished = walked.lp.send("hi").await.unwrap();
    let failed = spent.lp.send("hi").await;

    assert_eq!(finished, StopCondition::EndTurn);
    let route: Vec<_> = switches(&mut walked.events)
        .into_iter()
        .map(|(from, to, _)| (from, to))
        .collect();
    assert_eq!(
        route,
        [
            (PRIMARY.to_owned(), SECOND.to_owned()),
            (SECOND.to_owned(), THIRD.to_owned()),
        ]
    );
    assert!(matches!(
        failed,
        Err(TurnError::Provider(ProviderError::Retryable { .. }))
    ));
    assert_eq!(switches(&mut spent.events).len(), 2);
}

#[tokio::test]
async fn the_budget_counts_the_tokens_of_both_models() {
    let mut rig = rig(
        vec![note_call(), rate_limited()],
        vec![(SECOND, vec![answer("done")])],
    );

    rig.lp.send("hi").await.unwrap();

    assert_eq!(rig.lp.used_tokens(), 2 * TOKENS_PER_REPLY);
}

#[tokio::test]
async fn the_turn_stays_on_the_fallback_and_the_next_send_starts_on_the_primary() {
    let mut rig = rig(
        vec![note_call(), rate_limited(), answer("back")],
        vec![(SECOND, vec![answer("done")])],
    );

    rig.lp.send("one").await.unwrap();
    let after_first = (calls(&rig.primary), calls(&rig.fallbacks[0]));
    rig.lp.send("two").await.unwrap();

    assert_eq!(after_first, (2, 1));
    assert_eq!((calls(&rig.primary), calls(&rig.fallbacks[0])), (3, 1));
    assert_eq!(switches(&mut rig.events).len(), 1);
}
