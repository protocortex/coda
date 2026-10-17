// SPDX-License-Identifier: AGPL-3.0-only

//! Maps Anthropic stream events to [`ModelEvent`]s.

use std::collections::HashMap;

use serde_json::Value;

use super::classify_stream_error;
use crate::model::{ModelEvent, ProviderError, StopReason, Usage};

/// Arguments of a tool call that sent none, so they still parse as JSON.
const EMPTY_ARGUMENTS: &str = "{}";

/// What one stream event produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Events to deliver, in order.
    pub events: Vec<ModelEvent>,
    /// Whether the message is complete.
    pub done: bool,
}

#[derive(Debug)]
struct ToolBlock {
    call_id: String,
    has_arguments: bool,
}

/// Turns one response's stream events into model events.
#[derive(Debug, Default)]
pub struct EventMapper {
    started: bool,
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<StopReason>,
    tools: HashMap<u64, ToolBlock>,
}

fn fatal(message: impl Into<String>) -> ProviderError {
    ProviderError::Fatal {
        message: message.into(),
    }
}

fn token_count(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

fn parse_stop_reason(reason: &str) -> Result<StopReason, ProviderError> {
    match reason {
        "end_turn" | "stop_sequence" => Ok(StopReason::EndTurn),
        "tool_use" => Ok(StopReason::ToolUse),
        "max_tokens" => Ok(StopReason::MaxTokens),
        "refusal" => Ok(StopReason::Refusal),
        "pause_turn" => Ok(StopReason::Paused),
        other => Err(fatal(format!(
            "Anthropic stopped with unsupported reason {other}"
        ))),
    }
}

impl EventMapper {
    /// Handles the JSON payload of one stream event.
    ///
    /// # Errors
    /// Returns the classified failure for an error event or a bad stream.
    pub fn handle(&mut self, data: &str) -> Result<Step, ProviderError> {
        let event: Value = serde_json::from_str(data)
            .map_err(|_| fatal("Anthropic sent a stream event that is not valid JSON"))?;
        let mut step = Step::default();
        match event["type"].as_str().unwrap_or_default() {
            "message_start" => self.start(&event),
            "content_block_start" => self.block_start(&event, &mut step),
            "content_block_delta" => self.block_delta(&event, &mut step),
            "content_block_stop" => self.block_stop(&event, &mut step),
            "message_delta" => self.message_delta(&event)?,
            "message_stop" => self.finish(&mut step)?,
            "error" => {
                let kind = event.pointer("/error/type").and_then(Value::as_str);
                let detail = event.pointer("/error/message").and_then(Value::as_str);
                return Err(classify_stream_error(
                    kind.unwrap_or("unknown"),
                    detail.unwrap_or_default(),
                ));
            }
            _ => {}
        }
        Ok(step)
    }

    /// Whether any output has been seen; a retry would repeat it.
    #[must_use]
    pub const fn output_started(&self) -> bool {
        self.started
    }

    fn start(&mut self, event: &Value) {
        self.started = true;
        self.input_tokens = [
            "input_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
        ]
        .iter()
        .map(|field| token_count(event, &format!("/message/usage/{field}")))
        .sum();
    }

    fn block_start(&mut self, event: &Value, step: &mut Step) {
        let block = &event["content_block"];
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str().filter(|text| !text.is_empty()) {
                    step.events.push(ModelEvent::TextDelta {
                        text: text.to_owned(),
                    });
                }
            }
            Some("tool_use") => {
                let call_id = block["id"].as_str().unwrap_or_default().to_owned();
                let name = block["name"].as_str().unwrap_or_default().to_owned();
                step.events.push(ModelEvent::ToolCallStart {
                    call_id: call_id.clone(),
                    name,
                });
                let tool = ToolBlock {
                    call_id,
                    has_arguments: false,
                };
                self.tools
                    .insert(event["index"].as_u64().unwrap_or_default(), tool);
            }
            _ => {}
        }
    }

    fn block_delta(&mut self, event: &Value, step: &mut Step) {
        let delta = &event["delta"];
        match delta["type"].as_str() {
            Some("text_delta") => {
                let text = delta["text"].as_str().unwrap_or_default().to_owned();
                step.events.push(ModelEvent::TextDelta { text });
            }
            Some("input_json_delta") => {
                let fragment = delta["partial_json"].as_str().unwrap_or_default();
                let tool = self
                    .tools
                    .get_mut(&event["index"].as_u64().unwrap_or_default());
                if let Some(tool) = tool.filter(|_| !fragment.is_empty()) {
                    tool.has_arguments = true;
                    step.events.push(ModelEvent::ToolCallArgumentsDelta {
                        call_id: tool.call_id.clone(),
                        fragment: fragment.to_owned(),
                    });
                }
            }
            _ => {}
        }
    }

    fn block_stop(&mut self, event: &Value, step: &mut Step) {
        let index = event["index"].as_u64().unwrap_or_default();
        if let Some(tool) = self.tools.remove(&index).filter(|tool| !tool.has_arguments) {
            step.events.push(ModelEvent::ToolCallArgumentsDelta {
                call_id: tool.call_id,
                fragment: EMPTY_ARGUMENTS.to_owned(),
            });
        }
    }

    fn message_delta(&mut self, event: &Value) -> Result<(), ProviderError> {
        if let Some(reason) = event.pointer("/delta/stop_reason").and_then(Value::as_str) {
            self.stop_reason = Some(parse_stop_reason(reason)?);
        }
        self.output_tokens = token_count(event, "/usage/output_tokens");
        Ok(())
    }

    fn finish(&mut self, step: &mut Step) -> Result<(), ProviderError> {
        let reason = self
            .stop_reason
            .ok_or_else(|| fatal("Anthropic ended the message without a stop reason"))?;
        step.events.push(ModelEvent::Usage(Usage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
        }));
        step.events.push(ModelEvent::Stop { reason });
        step.done = true;
        Ok(())
    }
}
