// SPDX-License-Identifier: AGPL-3.0-only

//! The `task` tool: the name, arguments and nesting rule of a sub-agent call.
//!
//! The tool is not a [`Tool`](crate::Tool). It starts a child conversation,
//! which needs the session's provider, permission gate and event log, so the
//! turn loop of `coda-core` dispatches it. It is also left out of
//! [`TOOL_NAMES`](crate::TOOL_NAMES): that list is what an agent definition may
//! allow, and no agent may start another.

use serde::Deserialize;
use serde_json::Value;

use crate::{ToolError, parse_args};

/// The name the model uses to call the tool.
pub const NAME: &str = "task";

/// What the model is told the tool does.
pub const DESCRIPTION: &str = "Run a named sub-agent on a prompt in a fresh conversation and \
return its result. The sub-agent sees only the prompt, so include everything it needs.";

/// The deepest a chain of sub-agents may go. A top-level session is at depth 0,
/// so 1 lets it start a child and keeps that child from starting another.
pub const MAX_DEPTH: u8 = 1;

/// The arguments of a `task` call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskArgs {
    /// The agent to run, by name.
    pub agent: String,
    /// What the agent is asked to do.
    pub prompt: String,
}

/// A session at the depth limit asked for another level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("sub-agents cannot start sub-agents: depth {depth} is at the limit of {MAX_DEPTH}")]
pub struct DepthExceeded {
    /// The depth of the session that asked.
    pub depth: u8,
}

/// Reads the arguments of a `task` call.
///
/// # Errors
/// Returns [`ToolError::InvalidInput`] when a field is missing, unknown or not a string.
pub fn parse(input: &Value) -> Result<TaskArgs, ToolError> {
    parse_args(NAME, input)
}

/// The depth of a child started by a session at `parent_depth`.
///
/// # Errors
/// Returns [`DepthExceeded`] when the child would be deeper than [`MAX_DEPTH`].
pub fn child_depth(parent_depth: u8) -> Result<u8, DepthExceeded> {
    if parent_depth >= MAX_DEPTH {
        return Err(DepthExceeded {
            depth: parent_depth,
        });
    }
    Ok(parent_depth + 1)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{DepthExceeded, MAX_DEPTH, NAME, TaskArgs, child_depth, parse};
    use crate::{TOOL_NAMES, ToolError};

    #[test]
    fn the_arguments_are_an_agent_name_and_a_prompt_and_nothing_else() {
        // Arrange
        let good = json!({"agent": "explore", "prompt": "find the parser"});
        let extra = json!({"agent": "explore", "prompt": "p", "tools": ["bash"]});

        // Act
        let (parsed, missing, unknown) =
            (parse(&good), parse(&json!({"agent": "x"})), parse(&extra));

        // Assert
        let args = TaskArgs {
            agent: "explore".to_owned(),
            prompt: "find the parser".to_owned(),
        };
        assert_eq!(parsed.unwrap(), args);
        for refused in [missing, unknown] {
            assert!(matches!(
                refused,
                Err(ToolError::InvalidInput { tool: NAME, .. })
            ));
        }
        assert!(
            !TOOL_NAMES.contains(&NAME),
            "an agent may not be given task"
        );
    }

    #[test]
    fn only_a_session_below_the_cap_may_start_a_child() {
        // Act
        let below = child_depth(MAX_DEPTH - 1);
        let at = child_depth(MAX_DEPTH);
        let beyond = child_depth(MAX_DEPTH + 1);

        // Assert
        assert_eq!(below, Ok(MAX_DEPTH));
        assert_eq!(at, Err(DepthExceeded { depth: MAX_DEPTH }));
        assert_eq!(
            beyond,
            Err(DepthExceeded {
                depth: MAX_DEPTH + 1
            })
        );
    }
}
