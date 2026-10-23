// SPDX-License-Identifier: AGPL-3.0-only

//! The chat completions request body.

use coda_config::config::MaxTokensField;
use serde_json::{Value, json};

use crate::anthropic::request::RequestSettings;
use crate::model::{ModelRequest, RequestMessage, RequestToolCall, ToolSchema};

/// Chat completions has no error flag on a tool message, so a failed call is
/// marked in its text.
const TOOL_ERROR_MARKER: &str = "[tool error] ";

/// The parts of the request body that servers disagree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WireOptions {
    /// The key that carries the output token limit.
    pub max_tokens_field: MaxTokensField,
    /// Whether the request asks the server to report usage at the end of the
    /// stream.
    pub stream_usage: bool,
}

impl Default for WireOptions {
    fn default() -> Self {
        Self {
            max_tokens_field: MaxTokensField::default(),
            stream_usage: true,
        }
    }
}

/// Builds the JSON body of a streaming chat completions request.
///
/// A non-empty system prompt or tool list on the request replaces the one in
/// `settings`. Tools are sorted by name so the prompt prefix is byte-stable.
/// Usage is requested explicitly because servers omit it from a stream
/// otherwise; a server that rejects the option needs `wire.stream_usage` off.
#[must_use]
pub fn build_body(settings: &RequestSettings, wire: WireOptions, request: &ModelRequest) -> Value {
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
        "stream": true,
        "messages": messages,
    });
    body[wire.max_tokens_field.as_str()] = json!(settings.max_tokens);
    if wire.stream_usage {
        body["stream_options"] = json!({"include_usage": true});
    }
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

#[cfg(test)]
mod tests {
    use coda_config::config::MaxTokensField;
    use serde_json::{Value, json};

    use super::{WireOptions, build_body};
    use crate::anthropic::request::RequestSettings;
    use crate::model::ModelRequest;

    const LIMIT: u32 = 512;

    fn body(wire: WireOptions) -> Value {
        let settings = RequestSettings {
            model: "m".to_owned(),
            max_tokens: LIMIT,
            system: String::new(),
            tools: Vec::new(),
        };
        build_body(&settings, wire, &ModelRequest::user_prompt("hi"))
    }

    #[test]
    fn the_default_body_has_max_tokens_and_asks_for_stream_usage() {
        // Arrange
        let wire = WireOptions::default();

        // Act
        let body = body(wire);

        // Assert
        assert_eq!(body["max_tokens"], LIMIT);
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert_eq!(body.get("max_completion_tokens"), None);
    }

    #[test]
    fn max_completion_tokens_replaces_max_tokens() {
        // Arrange
        let wire = WireOptions {
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..WireOptions::default()
        };

        // Act
        let body = body(wire);

        // Assert
        assert_eq!(body["max_completion_tokens"], LIMIT);
        assert_eq!(body.get("max_tokens"), None);
    }

    #[test]
    fn stream_usage_off_leaves_out_stream_options() {
        // Arrange
        let wire = WireOptions {
            stream_usage: false,
            ..WireOptions::default()
        };

        // Act
        let body = body(wire);

        // Assert
        assert_eq!(body.get("stream_options"), None);
        assert_eq!(body["stream"], true);
    }
}
