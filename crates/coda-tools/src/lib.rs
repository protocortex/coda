// SPDX-License-Identifier: AGPL-3.0-only

//! Read-only workspace tools and the [`Tool`] trait they implement.
//!
//! `Tool` is synchronous and object-safe: file access is blocking, so async
//! callers (the broker, the core loop) hold `Arc<dyn Tool>` and run `call`
//! inside `spawn_blocking`. Every path a tool touches goes through
//! [`sandbox::Sandbox`].

pub mod grep;
pub mod read;
pub mod sandbox;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::sandbox::SandboxError;

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
}

impl ToolOutput {
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
