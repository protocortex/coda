// SPDX-License-Identifier: AGPL-3.0-only

//! The behaviour every [`ModelProvider`] must show, as a suite any provider
//! crate can run against its own implementation.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::model::{
    ModelEvent, ModelProvider, ModelRequest, ProviderError, RequestMessage, RequestToolCall,
    StopReason, ToolSchema,
};

/// A situation the suite puts a provider in. The factory passed to
/// [`run_contract`] must return a provider that will act it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Streams text and stops with a final answer.
    Text,
    /// Streams a tool call whose arguments arrive in several fragments.
    ToolCall,
    /// Fails because the request is too long for the model.
    ContextWindowExceeded,
    /// Fails with a transient error.
    Retryable,
    /// Fails with an error retrying cannot fix.
    Fatal,
    /// The stream is dropped mid-response; run by [`run_drop_contract`].
    DropCancels,
}

impl Scenario {
    const ALL: [Self; 5] = [
        Self::Text,
        Self::ToolCall,
        Self::ContextWindowExceeded,
        Self::Retryable,
        Self::Fatal,
    ];
}

/// A rule of the contract that a provider broke.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("scenario {scenario:?}: {reason}")]
pub struct ContractViolation {
    /// The scenario that exposed the problem.
    pub scenario: Scenario,
    /// Which rule was broken.
    pub reason: String,
}

/// Everything one stream delivered.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Collected {
    /// The events, in order.
    pub events: Vec<ModelEvent>,
    /// The failure, whether returned on open or delivered mid-stream.
    pub error: Option<ProviderError>,
    /// Whether the stream stayed open past the timeout.
    pub timed_out: bool,
}

/// Opens a stream and drains it, giving up after `timeout`.
pub async fn collect<P: ModelProvider>(
    provider: &P,
    request: ModelRequest,
    timeout: Duration,
) -> Collected {
    let drain = async {
        let mut collected = Collected::default();
        match provider.stream(request).await {
            Err(error) => collected.error = Some(error),
            Ok(mut stream) => {
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(event) => collected.events.push(event),
                        Err(error) => {
                            collected.error = Some(error);
                            break;
                        }
                    }
                }
            }
        }
        collected
    };
    // Events gathered before a hang are lost; the violation is the hang.
    tokio::time::timeout(timeout, drain)
        .await
        .unwrap_or_else(|_| Collected {
            timed_out: true,
            ..Collected::default()
        })
}

/// Runs every scenario against the provider `factory` builds for it.
///
/// # Errors
/// Returns the first [`ContractViolation`] found.
pub async fn run_contract<P, F>(factory: F, timeout: Duration) -> Result<(), ContractViolation>
where
    P: ModelProvider,
    F: Fn(Scenario) -> P,
{
    for scenario in Scenario::ALL {
        let provider = factory(scenario);
        let collected = collect(&provider, structured_request(), timeout).await;
        check(scenario, &collected).map_err(|reason| ContractViolation { scenario, reason })?;
    }
    Ok(())
}

/// A request with a prefix and a tool round trip, which every provider must accept.
fn structured_request() -> ModelRequest {
    ModelRequest {
        system: "contract".to_owned(),
        tools: vec![ToolSchema {
            name: "echo".to_owned(),
            description: "echoes".to_owned(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        messages: vec![
            RequestMessage::User("contract".to_owned()),
            RequestMessage::Assistant {
                text: String::new(),
                tool_calls: vec![RequestToolCall {
                    id: "call-1".to_owned(),
                    name: "echo".to_owned(),
                    arguments: "{}".to_owned(),
                }],
            },
            RequestMessage::ToolResult {
                call_id: "call-1".to_owned(),
                text: "done".to_owned(),
                is_error: false,
            },
        ],
    }
}

fn check(scenario: Scenario, collected: &Collected) -> Result<(), String> {
    if collected.timed_out {
        return Err("the stream neither ended nor failed in time".to_owned());
    }
    match scenario {
        Scenario::Text => check_success(collected, StopReason::EndTurn),
        Scenario::ToolCall => {
            check_success(collected, StopReason::ToolUse)?;
            check_tool_calls(&collected.events)
        }
        Scenario::ContextWindowExceeded => {
            expect_error(collected, |e| *e == ProviderError::ContextWindowExceeded)
        }
        Scenario::Retryable => {
            expect_error(collected, |e| matches!(e, ProviderError::Retryable { .. }))
        }
        Scenario::Fatal => expect_error(collected, |e| matches!(e, ProviderError::Fatal { .. })),
        Scenario::DropCancels => Err("the drop scenario is run by run_drop_contract".to_owned()),
    }
}

fn expect_error(
    collected: &Collected,
    accepts: impl Fn(&ProviderError) -> bool,
) -> Result<(), String> {
    match &collected.error {
        Some(error) if accepts(error) => Ok(()),
        Some(error) => Err(format!("failed with the wrong error class: {error}")),
        None => Err("expected a failure but the stream succeeded".to_owned()),
    }
}

fn check_success(collected: &Collected, expected: StopReason) -> Result<(), String> {
    if let Some(error) = &collected.error {
        return Err(format!("failed unexpectedly: {error}"));
    }
    let stops: Vec<usize> = collected
        .events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, ModelEvent::Stop { .. }))
        .map(|(index, _)| index)
        .collect();
    match stops.as_slice() {
        [] => return Err("the stream ended without a stop event".to_owned()),
        [index] if *index + 1 != collected.events.len() => {
            return Err("events were sent after the stop event".to_owned());
        }
        [index] => match &collected.events[*index] {
            ModelEvent::Stop { reason } if *reason == expected => {}
            other => return Err(format!("expected stop reason {expected:?}, got {other:?}")),
        },
        _ => return Err("more than one stop event".to_owned()),
    }
    let usages = collected
        .events
        .iter()
        .filter(|event| matches!(event, ModelEvent::Usage(_)))
        .count();
    match usages {
        0 => Err("the stream had no usage event".to_owned()),
        1 => Ok(()),
        _ => Err("the stream had more than one usage event".to_owned()),
    }
}

fn check_tool_calls(events: &[ModelEvent]) -> Result<(), String> {
    let mut open: Vec<(&str, String)> = Vec::new();
    let mut seen = HashSet::new();
    for event in events {
        match event {
            ModelEvent::ToolCallStart { call_id, .. } => {
                if !seen.insert(call_id.as_str()) {
                    return Err(format!("tool call {call_id} started twice"));
                }
                open.push((call_id, String::new()));
            }
            ModelEvent::ToolCallArgumentsDelta { call_id, fragment } => {
                let call = open
                    .iter_mut()
                    .find(|(id, ..)| id == call_id)
                    .ok_or_else(|| format!("arguments for {call_id} arrived before its start"))?;
                call.1.push_str(fragment);
            }
            _ => {}
        }
    }
    if open.is_empty() {
        return Err("no tool call was streamed".to_owned());
    }
    for (call_id, arguments) in open {
        if serde_json::from_str::<serde_json::Value>(&arguments).is_err() {
            return Err(format!("tool call {call_id} arguments are not valid JSON"));
        }
    }
    Ok(())
}

/// Held by a provider's background work so the suite can tell whether it is
/// still running.
#[derive(Debug)]
pub struct ActivityToken {
    _alive: Arc<()>,
}

/// Observes whether any [`ActivityToken`] it issued is still alive.
#[derive(Debug, Default)]
pub struct ActivityProbe(Arc<()>);

impl ActivityProbe {
    /// A token to move into the provider's background work.
    #[must_use]
    pub fn token(&self) -> ActivityToken {
        ActivityToken {
            _alive: Arc::clone(&self.0),
        }
    }

    /// Whether a token is still alive.
    #[must_use]
    pub fn is_active(&self) -> bool {
        Arc::strong_count(&self.0) > 1
    }
}

/// Checks that dropping a stream stops the provider's background work.
///
/// `factory` receives a probe and must build a provider whose first stream
/// emits at least one event, then keeps working while holding a token from the
/// probe. The suite reads one event, drops the stream, and waits up to
/// `timeout` for every token to be released.
///
/// # Errors
/// Returns a [`ContractViolation`] when the work outlives the stream.
pub async fn run_drop_contract<P, F>(factory: F, timeout: Duration) -> Result<(), ContractViolation>
where
    P: ModelProvider,
    F: FnOnce(Arc<ActivityProbe>) -> P,
{
    let violation = |reason: &str| ContractViolation {
        scenario: Scenario::DropCancels,
        reason: reason.to_owned(),
    };
    let probe = Arc::new(ActivityProbe::default());
    let provider = factory(Arc::clone(&probe));
    let request = ModelRequest::user_prompt("contract");
    let mut stream = tokio::time::timeout(timeout, provider.stream(request))
        .await
        .map_err(|_| violation("the stream did not open in time"))?
        .map_err(|error| violation(&format!("failed to open: {error}")))?;
    tokio::time::timeout(timeout, stream.next())
        .await
        .map_err(|_| violation("the stream sent no event in time"))?;
    drop(stream);
    let stopped = tokio::time::timeout(timeout, async {
        while probe.is_active() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    stopped.map_err(|_| violation("background work kept running after the stream was dropped"))
}
