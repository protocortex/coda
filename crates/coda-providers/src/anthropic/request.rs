// SPDX-License-Identifier: Apache-2.0

//! The request body.

use serde_json::{Value, json};

use crate::model::{ModelRequest, ReasoningBlock, RequestMessage, RequestToolCall};

pub use crate::model::ToolSchema;

/// Request parts that stay the same across calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestSettings {
    /// The model id.
    pub model: String,
    /// Output token limit.
    pub max_tokens: u32,
    /// The system prompt; omitted when empty.
    pub system: String,
    /// The tools on offer; omitted when empty.
    pub tools: Vec<ToolSchema>,
}

fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

/// Builds the JSON body of a streaming Messages request.
///
/// A non-empty system prompt or tool list on the request replaces the one in
/// `settings`. The system block and the last tool carry cache breakpoints; the
/// API caches everything before a breakpoint, so the last tool covers the whole
/// tool list. Tools are sorted by name so the cached prefix is byte-stable.
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
    let mut body = json!({
        "model": settings.model,
        "max_tokens": settings.max_tokens,
        "stream": true,
        "messages": encode_messages(&request.messages),
    });
    if !system.is_empty() {
        body["system"] = json!([{
            "type": "text",
            "text": system,
            "cache_control": ephemeral(),
        }]);
    }
    if !tools.is_empty() {
        let mut sorted: Vec<&ToolSchema> = tools.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        let mut tools = json!(sorted);
        if let Some(last) = tools.as_array_mut().and_then(|tools| tools.last_mut()) {
            last["cache_control"] = ephemeral();
        }
        body["tools"] = tools;
    }
    body
}

/// Encodes the history. Consecutive tool results share one user message, as
/// the API requires every result of an assistant turn to arrive together.
fn encode_messages(messages: &[RequestMessage]) -> Vec<Value> {
    let mut encoded: Vec<Value> = Vec::with_capacity(messages.len());
    let mut last_was_result = false;
    let mut reasoning: &[ReasoningBlock] = &[];
    for message in messages {
        let held = std::mem::take(&mut reasoning);
        match message {
            RequestMessage::Reasoning(blocks) => reasoning = blocks,
            RequestMessage::User(text) => {
                encoded.push(json!({"role": "user", "content": text}));
                last_was_result = false;
            }
            RequestMessage::Assistant { text, tool_calls } => {
                let mut content = Vec::with_capacity(tool_calls.len() + held.len() + 1);
                content.extend(held.iter().filter_map(encode_reasoning));
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
                content.extend(tool_calls.iter().map(encode_tool_use));
                encoded.push(json!({"role": "assistant", "content": content}));
                last_was_result = false;
            }
            RequestMessage::ToolResult {
                call_id,
                text,
                is_error,
            } => {
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": text,
                });
                if *is_error {
                    block["is_error"] = json!(true);
                }
                match encoded.last_mut().and_then(|m| m["content"].as_array_mut()) {
                    Some(blocks) if last_was_result => blocks.push(block),
                    _ => encoded.push(json!({"role": "user", "content": [block]})),
                }
                last_was_result = true;
            }
        }
    }
    encoded
}

/// A reasoning block as the API wants it back. A thinking block with no
/// signature did not come from this API, which rejects it, so it is left out.
fn encode_reasoning(block: &ReasoningBlock) -> Option<Value> {
    match block {
        ReasoningBlock::Thinking {
            text,
            signature: Some(signature),
        } => Some(json!({"type": "thinking", "thinking": text, "signature": signature})),
        ReasoningBlock::Thinking {
            signature: None, ..
        } => None,
        ReasoningBlock::Redacted { data } => {
            Some(json!({"type": "redacted_thinking", "data": data}))
        }
    }
}

/// The API needs an object for `input`; arguments the model wrote as invalid
/// JSON are replaced by an empty object, since the loop already reported the
/// parse failure to the model as a tool error.
fn encode_tool_use(call: &RequestToolCall) -> Value {
    let input = serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    json!({"type": "tool_use", "id": call.id, "name": call.name, "input": input})
}
