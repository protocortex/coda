// SPDX-License-Identifier: AGPL-3.0-only

//! Maps chat completions stream chunks to [`ModelEvent`]s.

use serde_json::Value;

use super::{fatal, is_context_overflow, retryable};
use crate::model::{ModelEvent, ProviderError, StopReason, Usage};

const DONE_MARKER: &str = "[DONE]";
/// Arguments of a tool call that sent none, so they still parse as JSON.
const EMPTY_ARGUMENTS: &str = "{}";
pub const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_TOOL_CALLS: usize = 128;
/// Error kinds by which a server marks a failure worth retrying.
const RETRYABLE_ERROR_KINDS: [&str; 2] = ["server_error", "rate_limit"];

/// What one stream payload produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Events to deliver, in order.
    pub events: Vec<ModelEvent>,
    /// Whether the response is complete.
    pub done: bool,
}

#[derive(Debug)]
struct ToolCall {
    index: Option<u64>,
    call_id: String,
    argument_bytes: usize,
}

/// Turns one response's stream payloads into model events.
#[derive(Debug, Default)]
pub struct EventMapper {
    started: bool,
    usage: Option<Usage>,
    stop_reason: Option<StopReason>,
    tools: Vec<ToolCall>,
}

fn parse_stop_reason(reason: &str) -> Result<StopReason, ProviderError> {
    match reason {
        "stop" => Ok(StopReason::EndTurn),
        "tool_calls" | "function_call" => Ok(StopReason::ToolUse),
        "length" => Ok(StopReason::MaxTokens),
        "content_filter" => Ok(StopReason::Refusal),
        other => Err(fatal(format!(
            "the server stopped with unsupported reason {other}"
        ))),
    }
}

impl EventMapper {
    /// Handles the data of one stream event.
    ///
    /// # Errors
    /// Returns the classified failure for an error object or a bad stream.
    pub fn handle(&mut self, data: &str) -> Result<Step, ProviderError> {
        let mut step = Step::default();
        if data.trim() == DONE_MARKER {
            return self.finish();
        }
        let chunk: Value = serde_json::from_str(data)
            .map_err(|_| fatal("the server sent a stream event that is not valid JSON"))?;
        if let Some(error) = chunk.get("error") {
            return Err(classify_stream_error(error));
        }
        self.started = true;
        if let Some(usage) = chunk.get("usage").filter(|usage| usage.is_object()) {
            self.usage = Some(Usage {
                input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
                output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
            });
        }
        for choice in chunk["choices"].as_array().into_iter().flatten() {
            self.choice(choice, &mut step)?;
        }
        Ok(step)
    }

    /// Whether any output has been seen; a retry would repeat it.
    #[must_use]
    pub const fn output_started(&self) -> bool {
        self.started
    }

    fn choice(&mut self, choice: &Value, step: &mut Step) -> Result<(), ProviderError> {
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str().filter(|text| !text.is_empty()) {
            step.events.push(ModelEvent::TextDelta {
                text: text.to_owned(),
            });
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            self.tool_call(call, step)?;
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.stop_reason = Some(parse_stop_reason(reason)?);
        }
        Ok(())
    }

    /// The call a fragment continues: the one with its id, else the one at
    /// its index, else the latest. Servers differ in which of them they send.
    fn position(&self, id: Option<&str>, index: Option<u64>) -> Option<usize> {
        let matches = |tool: &ToolCall| match (id, index) {
            (Some(id), _) => tool.call_id == id,
            (None, Some(index)) => tool.index == Some(index),
            (None, None) => true,
        };
        self.tools.iter().rposition(matches)
    }

    fn tool_call(&mut self, call: &Value, step: &mut Step) -> Result<(), ProviderError> {
        let id = call["id"].as_str().filter(|id| !id.is_empty());
        let index = call["index"].as_u64();
        let at = if let Some(at) = self.position(id, index) {
            at
        } else {
            if self.tools.len() >= MAX_TOOL_CALLS {
                return Err(fatal(format!(
                    "the response holds more than {MAX_TOOL_CALLS} tool calls"
                )));
            }
            let call_id = id.map_or_else(|| self.synthesized_id(), str::to_owned);
            let name = call["function"]["name"].as_str().unwrap_or_default();
            step.events.push(ModelEvent::ToolCallStart {
                call_id: call_id.clone(),
                name: name.to_owned(),
            });
            self.tools.push(ToolCall {
                index,
                call_id,
                argument_bytes: 0,
            });
            self.tools.len() - 1
        };
        let fragment = call["function"]["arguments"].as_str().unwrap_or_default();
        if let Some(tool) = self.tools.get_mut(at).filter(|_| !fragment.is_empty()) {
            tool.argument_bytes += fragment.len();
            if tool.argument_bytes > MAX_ARGUMENT_BYTES {
                return Err(fatal(format!(
                    "a tool call has more than {MAX_ARGUMENT_BYTES} bytes of arguments"
                )));
            }
            step.events.push(ModelEvent::ToolCallArgumentsDelta {
                call_id: tool.call_id.clone(),
                fragment: fragment.to_owned(),
            });
        }
        Ok(())
    }

    /// An id for a call the server did not name, unused by any other call.
    fn synthesized_id(&self) -> String {
        let n = self.tools.len();
        // Of these `n + 1` candidates at most `n` are in use.
        (n..=2 * n)
            .map(|n| format!("call_{n}"))
            .find(|id| self.tools.iter().all(|tool| tool.call_id != *id))
            .unwrap_or_default()
    }

    /// Handles the connection closing without a `[DONE]`; a response that
    /// carried its finish reason is complete.
    ///
    /// # Errors
    /// Retryable when no finish reason was seen.
    pub fn end_of_stream(&mut self) -> Result<Step, ProviderError> {
        if self.stop_reason.is_none() {
            return Err(retryable(
                "the server closed the stream before a finish reason",
            ));
        }
        self.finish()
    }

    fn finish(&mut self) -> Result<Step, ProviderError> {
        let mut step = Step::default();
        let reason = match self
            .stop_reason
            .ok_or_else(|| fatal("the server ended the response without a finish reason"))?
        {
            StopReason::EndTurn if !self.tools.is_empty() => StopReason::ToolUse,
            other => other,
        };
        for tool in self.tools.iter().filter(|tool| tool.argument_bytes == 0) {
            step.events.push(ModelEvent::ToolCallArgumentsDelta {
                call_id: tool.call_id.clone(),
                fragment: EMPTY_ARGUMENTS.to_owned(),
            });
        }
        let usage = self.usage.unwrap_or(Usage {
            input_tokens: 0,
            output_tokens: 0,
        });
        step.events
            .extend([ModelEvent::Usage(usage), ModelEvent::Stop { reason }]);
        step.done = true;
        Ok(step)
    }
}

/// Classifies an error object the server sent inside a stream.
fn classify_stream_error(error: &Value) -> ProviderError {
    if is_context_overflow(error) {
        return ProviderError::ContextWindowExceeded;
    }
    let kind = error["type"].as_str().unwrap_or("unknown");
    let message = format!("the server reported a stream error of type {kind}");
    if RETRYABLE_ERROR_KINDS
        .iter()
        .any(|known| kind.contains(known))
    {
        retryable(message)
    } else {
        fatal(message)
    }
}
