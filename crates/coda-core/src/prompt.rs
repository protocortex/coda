// SPDX-License-Identifier: AGPL-3.0-only

//! The prompt a session sends: a byte-stable prefix and an append-only
//! message history.

use serde::Serialize;
use serde_json::Value;

/// One tool as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolSchema {
    /// The name the model calls the tool by.
    pub name: String,
    /// What the tool does.
    pub description: String,
    /// JSON Schema of the tool arguments.
    pub input_schema: Value,
}

/// Why a prefix could not be built.
#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    /// The prefix could not be serialized.
    #[error("cannot serialize the prompt prefix: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Serialize)]
struct PrefixView<'a> {
    system: &'a str,
    tools: &'a [ToolSchema],
    project_instructions: &'a str,
}

/// Rebuilds objects in key order so the result does not depend on how the
/// map type orders keys.
fn sort_keys(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, inner)| (key, sort_keys(inner)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_keys).collect()),
        other => other,
    }
}

/// The part of the prompt that never changes during a session: system prompt,
/// tool schemas and project instructions, serialized once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    bytes: Vec<u8>,
    system: String,
    tools: Vec<ToolSchema>,
}

impl Prefix {
    /// Serializes the prefix. Tools are ordered by name and object keys inside
    /// their schemas by key, so the same inputs always give the same bytes.
    ///
    /// # Errors
    ///
    /// Returns [`PromptError::Serialize`] when serialization fails.
    pub fn new(
        system: &str,
        tools: Vec<ToolSchema>,
        project_instructions: &str,
    ) -> Result<Self, PromptError> {
        let mut tools = tools;
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        for tool in &mut tools {
            tool.input_schema = sort_keys(std::mem::take(&mut tool.input_schema));
        }
        let bytes = serde_json::to_vec(&PrefixView {
            system,
            tools: &tools,
            project_instructions,
        })?;
        let system = if project_instructions.is_empty() {
            system.to_owned()
        } else {
            format!("{system}\n\n{project_instructions}")
        };
        Ok(Self {
            bytes,
            system,
            tools,
        })
    }

    /// The system prompt followed by the project instructions, as sent to the
    /// model.
    #[must_use]
    pub fn system(&self) -> &str {
        &self.system
    }

    /// The tool schemas in their sorted order.
    #[must_use]
    pub fn tools(&self) -> &[ToolSchema] {
        &self.tools
    }

    /// The serialized prefix.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// A tool call the model made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Identifies the call so its result can be matched.
    pub id: String,
    /// The tool called.
    pub name: String,
    /// The raw JSON arguments as the model wrote them.
    pub arguments: String,
}

/// One entry of the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// What the user said.
    User(String),
    /// What the model answered, with the tool calls it made.
    Assistant {
        /// The answer text.
        text: String,
        /// The calls it wants run.
        tool_calls: Vec<ToolCall>,
    },
    /// The outcome of one tool call.
    ToolResult {
        /// The call this answers.
        call_id: String,
        /// What the model sees.
        text: String,
        /// True when the call failed or was denied.
        is_error: bool,
    },
}

/// A prefix plus the messages exchanged so far. Messages are only appended,
/// so earlier ones never change.
#[derive(Debug, Clone)]
pub struct Conversation {
    prefix: Prefix,
    messages: Vec<Message>,
}

impl Conversation {
    /// Starts an empty conversation behind `prefix`.
    #[must_use]
    pub fn new(prefix: Prefix) -> Self {
        Self {
            prefix,
            messages: Vec::new(),
        }
    }

    /// Appends a message.
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// The prefix.
    #[must_use]
    pub fn prefix(&self) -> &Prefix {
        &self.prefix
    }

    /// The messages in the order they were appended.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const GOLDEN: &str = include_str!("../tests/golden/prefix.json");

    fn read_tool() -> ToolSchema {
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

    fn grep_tool() -> ToolSchema {
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

    fn prefix(tools: Vec<ToolSchema>) -> Prefix {
        Prefix::new("You are Coda.", tools, "Use tabs.").unwrap()
    }

    #[test]
    fn prefix_bytes_equal_the_golden_snapshot() {
        // Arrange
        let tools = vec![read_tool(), grep_tool()];

        // Act
        let prefix = prefix(tools);

        // Assert
        assert_eq!(prefix.bytes(), GOLDEN.as_bytes());
    }

    #[test]
    fn prefix_is_identical_across_turns_with_different_history() {
        // Arrange
        let mut conversation = Conversation::new(prefix(vec![read_tool(), grep_tool()]));
        let mut seen = vec![conversation.prefix().bytes().to_vec()];

        // Act
        conversation.push(Message::User("fix /tmp/a at noon".into()));
        seen.push(conversation.prefix().bytes().to_vec());
        conversation.push(Message::ToolResult {
            call_id: "c1".into(),
            text: "output from /var/x".into(),
            is_error: false,
        });
        seen.push(conversation.prefix().bytes().to_vec());

        // Assert
        for bytes in seen {
            assert_eq!(bytes, GOLDEN.as_bytes());
        }
    }

    #[test]
    fn prefix_ignores_tool_and_nested_key_order() {
        // Arrange
        let mut shuffled = read_tool();
        shuffled.input_schema = serde_json::from_str(
            r#"{"required":["path"],"properties":{"path":{"type":"string"}},"type":"object"}"#,
        )
        .unwrap();

        // Act
        let prefix = prefix(vec![shuffled, grep_tool()]);

        // Assert
        assert_eq!(prefix.bytes(), GOLDEN.as_bytes());
    }

    #[test]
    fn appending_never_changes_earlier_messages() {
        // Arrange
        let mut conversation = Conversation::new(prefix(vec![read_tool()]));
        conversation.push(Message::User("first".into()));
        let before = conversation.messages().to_vec();

        // Act
        conversation.push(Message::Assistant {
            text: "second".into(),
            tool_calls: vec![],
        });

        // Assert
        assert_eq!(&conversation.messages()[..1], before.as_slice());
        assert_eq!(conversation.messages().len(), 2);
    }
}
