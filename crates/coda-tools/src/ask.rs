// SPDX-License-Identifier: Apache-2.0

//! The `ask` tool: one short question for the user, with two to four options.
//!
//! The tool is not a [`Tool`](crate::Tool). The answer comes from a person, so
//! the turn loop of `coda-core` puts the question through the session's
//! permission path. It is left out of [`TOOL_NAMES`](crate::TOOL_NAMES), so no
//! agent definition may list it, and a sub-agent cannot ask the user directly.

use serde::Deserialize;
use serde_json::Value;

use crate::{ToolError, parse_args};

/// The name the model uses to call the tool.
pub const NAME: &str = "ask";

/// What the model is told the tool does.
pub const DESCRIPTION: &str = "Ask the user one short question and wait for the answer. Give \
two to four options; the reply names the option the user picked, or says the user did not \
choose. Ask only when you cannot go on without the user's choice.";

/// Longest question, in characters.
pub const MAX_QUESTION_CHARS: usize = 300;

/// Fewest options a question has, since one option is no choice.
pub const MIN_OPTIONS: usize = 2;

/// Most options a question has.
pub const MAX_OPTIONS: usize = 4;

/// Longest option, in characters.
pub const MAX_OPTION_CHARS: usize = 80;

/// The arguments of an `ask` call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskArgs {
    /// What the user is asked.
    pub question: String,
    /// What the user may pick, in order.
    pub options: Vec<String>,
}

/// Reads the arguments of an `ask` call.
///
/// # Errors
/// Returns [`ToolError::InvalidInput`] when a field is missing, unknown or of
/// the wrong kind, the question is blank or longer than [`MAX_QUESTION_CHARS`],
/// there are fewer than [`MIN_OPTIONS`] or more than [`MAX_OPTIONS`] options,
/// or an option is blank or longer than [`MAX_OPTION_CHARS`].
pub fn parse(input: &Value) -> Result<AskArgs, ToolError> {
    let mut args: AskArgs = parse_args(NAME, input)?;
    args.question = args.question.trim().to_owned();
    if !fits(&args.question, MAX_QUESTION_CHARS) {
        return Err(invalid(format!(
            "the question must hold 1 to {MAX_QUESTION_CHARS} characters"
        )));
    }
    if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&args.options.len()) {
        return Err(invalid(format!(
            "give {MIN_OPTIONS} to {MAX_OPTIONS} options"
        )));
    }
    for option in &mut args.options {
        *option = option.trim().to_owned();
        if !fits(option, MAX_OPTION_CHARS) {
            return Err(invalid(format!(
                "each option must hold 1 to {MAX_OPTION_CHARS} characters"
            )));
        }
    }
    Ok(args)
}

fn fits(text: &str, longest: usize) -> bool {
    (1..=longest).contains(&text.chars().count())
}

fn invalid(reason: String) -> ToolError {
    ToolError::InvalidInput { tool: NAME, reason }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::TOOL_NAMES;

    #[test]
    fn a_question_with_two_to_four_options_parses_trimmed() {
        let parsed = parse(&json!({"question": " Which? ", "options": [" a ", "b"]}));

        let args = parsed.unwrap();
        assert_eq!(args.question, "Which?");
        assert_eq!(args.options, ["a", "b"]);
        assert!(parse(&json!({"question": "q", "options": ["a", "b", "c", "d"]})).is_ok());
        assert!(!TOOL_NAMES.contains(&NAME), "an agent may not be given ask");
    }

    #[test]
    fn anything_else_is_refused_with_a_reason_the_model_can_act_on() {
        let too_long = "x".repeat(MAX_QUESTION_CHARS + 1);
        let long_option = "x".repeat(MAX_OPTION_CHARS + 1);
        for refused in [
            json!({}),
            json!({"question": "q"}),
            json!({"question": "  ", "options": ["a", "b"]}),
            json!({"question": too_long, "options": ["a", "b"]}),
            json!({"question": "q", "options": ["a"]}),
            json!({"question": "q", "options": ["a", "b", "c", "d", "e"]}),
            json!({"question": "q", "options": ["a", " "]}),
            json!({"question": "q", "options": ["a", long_option]}),
            json!({"question": "q", "options": ["a", "b"], "timeout": 5}),
        ] {
            let result = parse(&refused);
            assert!(
                matches!(result, Err(ToolError::InvalidInput { tool: NAME, .. })),
                "{refused}: {result:?}"
            );
        }
    }
}
