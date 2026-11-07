// SPDX-License-Identifier: Apache-2.0

//! The normalized tool request that backend adapters produce.

use std::path::PathBuf;

use coda_tools::{git, skill};

/// A tool a backend asks permission to run, after name mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tool {
    /// Run a shell command.
    Bash,
    /// Change files.
    Edit,
    /// Create a file, or replace one that was read.
    Write,
    /// Change several places, possibly in several files, as one call.
    MultiEdit,
    /// Read a file.
    Read,
    /// Search file contents or names.
    Grep,
    /// List a directory, or search it by name pattern.
    List,
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
    /// Show the working tree's status; read only.
    GitStatus,
    /// Show the working tree's diff; read only.
    GitDiff,
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
            Self::Write => "write",
            Self::MultiEdit => "multi_edit",
            Self::Read => "read",
            Self::Grep => "grep",
            Self::List => "list",
            Self::CodeReadRange => "code_read_range",
            Self::Retrieve => "retrieve",
            Self::CodeSearch => "code_search",
            Self::CodeSymbol => "code_symbol",
            Self::CodeOutline => "code_outline",
            Self::Skill => skill::NAME,
            Self::GitStatus => git::STATUS_NAME,
            Self::GitDiff => git::DIFF_NAME,
            Self::Unknown(name) => name,
        }
    }
}

/// Most characters of a backend's reason that are kept.
pub const MAX_REASON_CHARS: usize = 500;

/// The reason a backend gave for a request, trimmed and cut to
/// [`MAX_REASON_CHARS`]. Blank text gives `None`.
#[must_use]
pub fn reason_from(text: Option<&str>) -> Option<String> {
    let trimmed = text?.trim();
    (!trimmed.is_empty()).then(|| trimmed.chars().take(MAX_REASON_CHARS).collect())
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
    /// Why the backend asks, in its own words, when it says.
    pub reason: Option<String>,
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
            (Tool::Write, "write"),
            (Tool::MultiEdit, "multi_edit"),
            (Tool::Read, "read"),
            (Tool::Grep, "grep"),
            (Tool::List, "list"),
            (Tool::CodeReadRange, "code_read_range"),
            (Tool::Retrieve, "retrieve"),
            (Tool::CodeSearch, "code_search"),
            (Tool::CodeSymbol, "code_symbol"),
            (Tool::CodeOutline, "code_outline"),
            (Tool::Skill, "skill"),
            (Tool::GitStatus, "git_status"),
            (Tool::GitDiff, "git_diff"),
        ];

        for (tool, expected) in cases {
            // Act
            let name = tool.name();

            // Assert
            assert_eq!(name, expected);
        }
    }

    #[test]
    fn a_reason_is_trimmed_and_blank_or_missing_text_gives_none() {
        // Act
        let kept = reason_from(Some("  May I?\n"));
        let blank = reason_from(Some("  \n"));
        let missing = reason_from(None);

        // Assert
        assert_eq!(kept.as_deref(), Some("May I?"));
        assert_eq!((blank, missing), (None, None));
    }

    #[test]
    fn a_long_reason_is_cut_on_a_character_boundary() {
        // Arrange
        let long = "é".repeat(MAX_REASON_CHARS + 10);

        // Act
        let kept = reason_from(Some(&long)).unwrap();

        // Assert
        assert_eq!(kept.chars().count(), MAX_REASON_CHARS);
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
