// SPDX-License-Identifier: AGPL-3.0-only

//! Read-only workspace tools and the [`Tool`] trait they implement.
//!
//! `Tool` is synchronous and object-safe: file access is blocking, so async
//! callers (the broker, the core loop) hold `Arc<dyn Tool>` and run `call`
//! inside `spawn_blocking`. Every path a tool touches goes through
//! [`sandbox::Sandbox`].

pub mod bash;
pub mod code;
pub mod edit;
pub mod grep;
pub mod read;
pub mod retrieve;
pub mod sandbox;

use std::sync::atomic::AtomicBool;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::sandbox::SandboxError;

/// The name of every tool the model can call, sorted. Agent definitions are
/// checked against it.
pub const TOOL_NAMES: [&str; 9] = [
    bash::NAME,
    code::OUTLINE_NAME,
    code::READ_RANGE_NAME,
    code::SEARCH_NAME,
    code::SYMBOL_NAME,
    edit::NAME,
    grep::NAME,
    read::NAME,
    retrieve::NAME,
];

/// Default cap on the bytes a tool returns in one call.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// What a tool returns to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// The text handed back to the model, at most the tool's output cap.
    pub text: String,
    /// True when more output existed than the cap allowed.
    pub truncated: bool,
}

/// Why a tool call failed.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The arguments do not match the tool's schema.
    #[error("invalid arguments for tool {tool}: {reason}")]
    InvalidInput {
        /// The tool that rejected the arguments.
        tool: &'static str,
        /// What is wrong with them.
        reason: String,
    },
    /// The sandbox refused or failed to open a path.
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    /// Reading an already opened file failed.
    #[error("reading {path} failed: {source}")]
    Io {
        /// The workspace-relative path being read.
        path: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The text to replace does not occur in the file.
    #[error("old_string was not found in {path}")]
    NoMatch {
        /// The workspace-relative path that was searched.
        path: String,
    },
    /// The text to replace occurs more than once, so the edit is ambiguous.
    #[error(
        "old_string matches {count} places in {path}; add surrounding context to make it unique"
    )]
    NotUnique {
        /// The workspace-relative path that was searched.
        path: String,
        /// How many places matched.
        count: usize,
    },
    /// The file is bigger than the tool is willing to rewrite.
    #[error("{path} is larger than the {limit} byte edit limit")]
    TooLarge {
        /// The workspace-relative path.
        path: String,
        /// The size limit in bytes.
        limit: usize,
    },
    /// The shell could not be started.
    #[error("starting the shell failed: {0}")]
    Spawn(std::io::Error),
    /// The file changed after it was read, so the edit was not applied.
    #[error("{path} changed while it was being edited; read it again and retry")]
    Conflict {
        /// The workspace-relative path.
        path: String,
    },
    /// The original could not be retrieved.
    #[error(transparent)]
    Original(#[from] coda_protocol::readers::ReaderError),
    /// The code index could not answer.
    #[error("code index failed: {0}")]
    Index(coda_protocol::readers::ReaderError),
    /// The requested first line is after the last line of the file.
    #[error("{path} has {lines} lines, so line {start} is past the end")]
    PastEndOfFile {
        /// The workspace-relative path.
        path: String,
        /// The one-based line the caller asked to start at.
        start: u64,
        /// How many lines the file has.
        lines: u64,
    },
    /// The file is not valid UTF-8 text.
    #[error("{path} is not valid UTF-8 text")]
    NotUtf8 {
        /// The workspace-relative path.
        path: String,
    },
}

/// A capability the model can invoke.
pub trait Tool: Send + Sync {
    /// The name the model uses to call the tool.
    fn name(&self) -> &'static str;

    /// Runs the tool on JSON arguments.
    ///
    /// # Errors
    ///
    /// Returns [`ToolError`] when the arguments are invalid, the sandbox
    /// rejects a path, or reading fails.
    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError>;

    /// Like [`call`](Tool::call), but a long-running tool should stop soon
    /// after `cancel` becomes true. Tools that finish quickly keep this default.
    ///
    /// # Errors
    ///
    /// Same as [`call`](Tool::call).
    fn call_cancellable(
        &self,
        input: &Value,
        cancel: &AtomicBool,
    ) -> Result<ToolOutput, ToolError> {
        let _ = cancel;
        self.call(input)
    }
}

impl ToolOutput {
    /// Cuts `bytes` to `cap`, dropping a character the cut split in half.
    ///
    /// Callers pass at most `cap + 1` bytes: one extra byte is how a cut is noticed.
    pub(crate) fn from_bytes(mut bytes: Vec<u8>, cap: usize) -> Self {
        let truncated = bytes.len() > cap;
        if truncated {
            bytes.truncate(cap);
            if let Err(err) = std::str::from_utf8(&bytes)
                && err.error_len().is_none()
            {
                bytes.truncate(err.valid_up_to());
            }
        }
        Self {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated,
        }
    }

    /// Wraps `text`, cutting it to at most `cap` bytes on a character boundary.
    pub(crate) fn capped(mut text: String, cap: usize) -> Self {
        let truncated = text.len() > cap;
        if truncated {
            let mut end = cap;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        Self { text, truncated }
    }
}

pub(crate) fn parse_args<T: DeserializeOwned>(
    tool: &'static str,
    input: &Value,
) -> Result<T, ToolError> {
    T::deserialize(input).map_err(|err| ToolError::InvalidInput {
        tool,
        reason: err.to_string(),
    })
}
