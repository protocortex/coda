// SPDX-License-Identifier: AGPL-3.0-only

//! The chat completions request body.

use serde_json::{Value, json};

use crate::anthropic::request::RequestSettings;
use crate::model::{ModelRequest, RequestMessage, RequestToolCall, ToolSchema};

/// Chat completions has no error flag on a tool message, so a failed call is
/// marked in its text.
const TOOL_ERROR_MARKER: &str = "[tool error] ";

/// Builds the JSON body of a streaming chat completions request.
///
/// A non-empty system prompt or tool list on the request replaces the one in
/// `settings`. Tools are sorted by name so the prompt prefix is byte-stable.
/// Usage is requested explicitly because servers omit it from a stream
/// otherwise.
#[must_use]
pub fn build_body(settings: &RequestSettings, request: &ModelRequest) -> Value {
    let system = if request.system.is_empty() {
        &settings.system
    } else {
        &request.system
    };
    let tools = if request.tools.is_empty() {
        &settings.tools
    } else {
        &request.tools
    };
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    messages.extend(request.messages.iter().map(encode_message));
    let mut body = json!({
        "model": settings.model,
        "max_tokens": settings.max_tokens,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    if !tools.is_empty() {
        let mut sorted: Vec<&ToolSchema> = tools.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        body["tools"] = sorted.into_iter().map(encode_tool).collect();
    }
    body
}

fn encode_tool(tool: &ToolSchema) -> Value {
    json!({"type": "function", "function": {
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.input_schema,
    }})
}

fn encode_message(message: &RequestMessage) -> Value {
    match message {
        RequestMessage::User(text) => json!({"role": "user", "content": text}),
        RequestMessage::Assistant { text, tool_calls } => {
            let mut encoded = json!({"role": "assistant", "content": text});
            if !tool_calls.is_empty() {
                encoded["tool_calls"] = tool_calls.iter().map(encode_tool_call).collect();
            }
            encoded
        }
        RequestMessage::ToolResult {
            call_id,
            text,
            is_error,
        } => {
            let marker = if *is_error { TOOL_ERROR_MARKER } else { "" };
            json!({"role": "tool", "tool_call_id": call_id, "content": format!("{marker}{text}")})
        }
    }
}

fn encode_tool_call(call: &RequestToolCall) -> Value {
    json!({"id": call.id, "type": "function", "function": {
        "name": call.name,
        "arguments": call.arguments,
    }})
}
