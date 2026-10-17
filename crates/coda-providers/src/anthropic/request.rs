// SPDX-License-Identifier: AGPL-3.0-only

//! The request body.

use serde::Serialize;
use serde_json::{Value, json};

use crate::model::ModelRequest;

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolSchema {
    /// The tool name.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// JSON Schema of the arguments.
    pub input_schema: Value,
}

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
/// The system block and the last tool carry cache breakpoints; the API caches
/// everything before a breakpoint, so the last tool covers the whole tool list.
/// Tools are sorted by name so the cached prefix is byte-stable.
#[must_use]
pub fn build_body(settings: &RequestSettings, request: &ModelRequest) -> Value {
    let mut body = json!({
        "model": settings.model,
        "max_tokens": settings.max_tokens,
        "stream": true,
        "messages": [{"role": "user", "content": request.prompt}],
    });
    if !settings.system.is_empty() {
        body["system"] = json!([{
            "type": "text",
            "text": settings.system,
            "cache_control": ephemeral(),
        }]);
    }
    if !settings.tools.is_empty() {
        let mut sorted: Vec<&ToolSchema> = settings.tools.iter().collect();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        let mut tools = json!(sorted);
        if let Some(last) = tools.as_array_mut().and_then(|tools| tools.last_mut()) {
            last["cache_control"] = ephemeral();
        }
        body["tools"] = tools;
    }
    body
}
