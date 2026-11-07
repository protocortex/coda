// SPDX-License-Identifier: Apache-2.0

//! Recovery of tool calls from weak or local models: arguments that are almost
//! valid JSON, and a reply whose text is itself a call.
//!
//! Both are strict and deterministic. A repair only fixes what can be fixed
//! without guessing: it never invents a value, never closes a string and never
//! changes a key. A text-shaped call must match one tool exactly.

use serde_json::{Map, Value};

use crate::prompt::ToolSchema;

const SCHEMA_PROPERTIES: &str = "properties";
const SCHEMA_REQUIRED: &str = "required";
/// Keys of a call written as `{"name": ..., "arguments": {...}}`.
const WRAPPER_NAME: &str = "name";
const WRAPPER_ARGUMENTS: &str = "arguments";
const WRAPPER_KEYS: usize = 2;

/// A call recovered from the text of a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCall {
    /// The tool it names or fits.
    pub name: String,
    /// Its arguments, as JSON text.
    pub arguments: String,
}

/// `text` as valid JSON for an object, when it is not valid as it stands and
/// one of these fixes makes it so: a trailing comma, single-quoted strings, a
/// raw line break or tab inside a string, or closing braces and brackets cut off
/// the end. Anything else, such as an unquoted key, a string cut short, a value
/// missing after a colon, or text around the JSON, gives `None`.
#[must_use]
pub fn repair_arguments(text: &str) -> Option<String> {
    if serde_json::from_str::<Value>(text).is_ok() {
        return None;
    }
    let mut out = String::with_capacity(text.len() + 4);
    let mut closers: Vec<char> = Vec::new();
    let mut quote: Option<char> = None;
    let mut chars = text.trim().chars();
    while let Some(c) = chars.next() {
        if let Some(open) = quote {
            match c {
                '\\' => {
                    let next = chars.next()?;
                    if open == '\'' && next == '\'' {
                        out.push('\'');
                    } else {
                        out.push('\\');
                        out.push(next);
                    }
                }
                c if c == open => {
                    out.push('"');
                    quote = None;
                }
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => return None,
                c => out.push(c),
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                out.push('"');
            }
            '{' => {
                closers.push('}');
                out.push(c);
            }
            '[' => {
                closers.push(']');
                out.push(c);
            }
            '}' | ']' => {
                if !drop_trailing_comma(&mut out) || closers.pop() != Some(c) {
                    return None;
                }
                out.push(c);
            }
            c => out.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    // A cut can fall inside a number or a word, which would change the value.
    let ends_whole = out.trim_end().ends_with(['"', '}', ']', ',', '{', '[']);
    if !closers.is_empty() && !ends_whole {
        return None;
    }
    if !drop_trailing_comma(&mut out) {
        return None;
    }
    out.extend(closers.into_iter().rev());
    match serde_json::from_str::<Value>(&out) {
        Ok(Value::Object(_)) => Some(out),
        _ => None,
    }
}

/// Removes a comma at the end of `text`. Gives false when nothing but an
/// opening bracket, a colon or another comma comes before it: that comma
/// follows no value, so it is not a trailing comma.
fn drop_trailing_comma(text: &mut String) -> bool {
    let kept = text.trim_end().len();
    text.truncate(kept);
    if !text.ends_with(',') {
        return true;
    }
    text.pop();
    !text.trim_end().ends_with(['{', '[', ',', ':'])
}

/// The call a reply's `text` makes, when the whole text is one JSON object that
/// matches exactly one of `tools`: either the arguments alone, with every
/// required key present and no key the tool does not know, or
/// `{"name": <tool>, "arguments": {...}}` with the same fit. A reply that names
/// a tool that is not in `tools`, or fits two of them, makes no call.
#[must_use]
pub fn text_call(text: &str, tools: &[ToolSchema]) -> Option<TextCall> {
    let Value::Object(object) = serde_json::from_str(text.trim()).ok()? else {
        return None;
    };
    if let (Some(Value::String(name)), Some(arguments)) =
        (object.get(WRAPPER_NAME), object.get(WRAPPER_ARGUMENTS))
        && object.len() == WRAPPER_KEYS
    {
        let schema = tools.iter().find(|tool| tool.name == *name)?;
        let arguments = arguments.as_object()?;
        return fits(schema, arguments).then(|| call(schema, arguments));
    }
    if object.is_empty() {
        return None;
    }
    let mut fitting = tools.iter().filter(|tool| fits(tool, &object));
    let schema = fitting.next()?;
    fitting.next().is_none().then(|| call(schema, &object))
}

fn call(schema: &ToolSchema, arguments: &Map<String, Value>) -> TextCall {
    TextCall {
        name: schema.name.clone(),
        arguments: Value::Object(arguments.clone()).to_string(),
    }
}

/// Whether `arguments` has every required key of `tool` and no key it lacks.
fn fits(tool: &ToolSchema, arguments: &Map<String, Value>) -> bool {
    let Some(known) = tool
        .input_schema
        .get(SCHEMA_PROPERTIES)
        .and_then(Value::as_object)
    else {
        return false;
    };
    let required = tool
        .input_schema
        .get(SCHEMA_REQUIRED)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    arguments.keys().all(|key| known.contains_key(key))
        && required.into_iter().all(|key| arguments.contains_key(key))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn each_strict_repair_makes_the_arguments_valid() {
        // Arrange
        let cases = [
            (r#"{"path": "a.txt",}"#, r#"{"path": "a.txt"}"#),
            (r#"{"a": [1, 2,],}"#, r#"{"a": [1, 2]}"#),
            (r"{'path': 'a.txt'}", r#"{"path": "a.txt"}"#),
            (r#"{'text': 'say "hi"'}"#, r#"{"text": "say \"hi\""}"#),
            (r"{'text': 'it\'s'}", r#"{"text": "it's"}"#),
            (
                "{\"text\": \"one\ntwo\tthree\"}",
                r#"{"text": "one\ntwo\tthree"}"#,
            ),
            (r#"{"path": "a.txt""#, r#"{"path": "a.txt"}"#),
            (r#"{"a": {"b": [1, 2,"#, r#"{"a": {"b": [1, 2]}}"#),
            (r#"{"path": "a.txt","#, r#"{"path": "a.txt"}"#),
            (r#"{"offset": 1,"#, r#"{"offset": 1}"#),
        ];

        for (broken, fixed) in cases {
            // Act
            let repaired = repair_arguments(broken);

            // Assert
            assert_eq!(repaired.as_deref(), Some(fixed), "{broken}");
        }
    }

    #[test]
    fn what_cannot_be_fixed_without_guessing_is_left_alone() {
        // Arrange
        let cases = [
            "",
            "not json at all",
            r"{path: 'a.txt'}",
            r#"{"path": "a.tx"#,
            r#"{"path":"#,
            r#"{"path": "a.txt"} and more"#,
            r#"{"a": [1, 2}"#,
            r#"{"a": 1}}"#,
            r"{'text': 'it's'}",
            "```json\n{\"a\": 1}\n```",
            "[1, 2,]",
            r#"{"path": "a.txt", "offset": 1"#,
            r#"{"on": tru"#,
            r#"{"a": [,]}"#,
            "{,}",
            "{\"a\": \"x\u{1}y\"}",
        ];

        for broken in cases {
            // Act and Assert
            assert_eq!(repair_arguments(broken), None, "{broken:?}");
        }
    }

    #[test]
    fn valid_arguments_need_no_repair() {
        assert_eq!(repair_arguments(r#"{"a": 1}"#), None);
        assert_eq!(repair_arguments("{}"), None);
    }

    fn tool(name: &str, properties: &Value, required: &[&str]) -> ToolSchema {
        ToolSchema {
            name: name.to_owned(),
            description: String::new(),
            input_schema: json!({
                "type": "object",
                "properties": properties,
                "required": required,
            }),
        }
    }

    fn tools() -> Vec<ToolSchema> {
        vec![
            tool("read", &json!({"path": {}, "offset": {}}), &["path"]),
            tool("grep", &json!({"pattern": {}, "path": {}}), &["pattern"]),
            tool("list", &json!({"path": {}}), &[]),
        ]
    }

    #[test]
    fn an_object_with_the_keys_of_one_tool_only_is_that_tools_call() {
        // Arrange
        let text = r#" {"pattern": "fn main"} "#;

        // Act
        let found = text_call(text, &tools());

        // Assert
        assert_eq!(
            found,
            Some(TextCall {
                name: "grep".to_owned(),
                arguments: r#"{"pattern":"fn main"}"#.to_owned(),
            })
        );
    }

    #[test]
    fn a_call_written_with_its_name_is_accepted_when_the_arguments_fit() {
        // Arrange
        let text = r#"{"name": "read", "arguments": {"path": "a.txt", "offset": 3}}"#;

        // Act
        let found = text_call(text, &tools()).unwrap();

        // Assert
        assert_eq!(found.name, "read");
        assert_eq!(
            serde_json::from_str::<Value>(&found.arguments).unwrap(),
            json!({"path": "a.txt", "offset": 3})
        );
    }

    #[test]
    fn a_reply_that_is_not_one_unique_full_match_makes_no_call() {
        // Arrange
        let cases = [
            // Fits both `read` and `list`'s key set only through `path`, but read needs
            // nothing more than path and list nothing at all: two tools fit.
            r#"{"path": "a.txt"}"#,
            // A required key is missing.
            r#"{"offset": 1}"#,
            // A key the tool does not know.
            r#"{"pattern": "x", "flags": "i"}"#,
            // Empty object.
            "{}",
            // Not an object.
            r#"["read"]"#,
            "I will read the file",
            // Text around the JSON.
            r#"Sure: {"pattern": "x"}"#,
            // Names a tool that is not registered.
            r#"{"name": "rm", "arguments": {"path": "/"}}"#,
            r#"{"tool": "rm", "path": "/"}"#,
            // Names a registered tool but the arguments do not fit it.
            r#"{"name": "read", "arguments": {"pattern": "x"}}"#,
            // The arguments are not an object.
            r#"{"name": "read", "arguments": "a.txt"}"#,
        ];

        for text in cases {
            // Act and Assert
            assert_eq!(text_call(text, &tools()), None, "{text}");
        }
    }
}
