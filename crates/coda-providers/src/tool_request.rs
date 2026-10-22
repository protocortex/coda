// SPDX-License-Identifier: AGPL-3.0-only

//! The normalized tool request that backend adapters produce.

use std::path::PathBuf;

/// A tool a backend asks permission to run, after name mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    /// Run a shell command.
    Bash,
    /// Change files.
    Edit,
    /// Read a file.
    Read,
    /// Search file contents or names.
    Grep,
    /// Read a line range of an indexed file.
    CodeReadRange,
    /// Fetch a stored original.
    Retrieve,
    /// Search the code index.
    CodeSearch,
    /// Look up a symbol in the code index.
    CodeSymbol,
    /// Outline a file from the code index.
    CodeOutline,
    /// Load a skill's instructions from the session's definitions.
    Skill,
    /// A tool coda has no mapping for, with the backend's own name.
    Unknown(String),
}

impl Tool {
    /// The stable name used in events and grants.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Bash => "bash",
            Self::Edit => "edit",
            Self::Read => "read",
            Self::Grep => "grep",
            Self::CodeReadRange => "code_read_range",
            Self::Retrieve => "retrieve",
            Self::CodeSearch => "code_search",
            Self::CodeSymbol => "code_symbol",
            Self::CodeOutline => "code_outline",
            Self::Skill => "skill",
            Self::Unknown(name) => name,
        }
    }
}

/// One permission request, independent of the backend that raised it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRequest {
    /// The mapped tool.
    pub tool: Tool,
    /// Paths the tool reads or changes, as the backend gave them.
    pub target_paths: Vec<PathBuf>,
    /// The command line, for shell tools.
    pub command: Option<String>,
    /// Working directory the tool runs in.
    pub cwd: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tools_have_stable_snake_case_names() {
        // Arrange
        let cases = [
            (Tool::Bash, "bash"),
            (Tool::Edit, "edit"),
            (Tool::Read, "read"),
            (Tool::Grep, "grep"),
            (Tool::CodeReadRange, "code_read_range"),
            (Tool::Retrieve, "retrieve"),
            (Tool::CodeSearch, "code_search"),
            (Tool::CodeSymbol, "code_symbol"),
            (Tool::CodeOutline, "code_outline"),
            (Tool::Skill, "skill"),
        ];

        for (tool, expected) in cases {
            // Act
            let name = tool.name();

            // Assert
            assert_eq!(name, expected);
        }
    }

    #[test]
    fn unknown_tool_keeps_the_backend_name() {
        // Arrange
        let tool = Tool::Unknown("WebFetch".to_owned());

        // Act
        let name = tool.name();

        // Assert
        assert_eq!(name, "WebFetch");
    }
}
