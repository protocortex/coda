// SPDX-License-Identifier: Apache-2.0

//! The `[tool_servers.<name>]` tables: external programs that serve tools to a
//! model-API session over standard input and output. There is no server until
//! the user lists one in their own `config.toml`; a project file cannot add one.

use std::path::Path;

use serde::Deserialize;

/// Key of the tables in `config.toml`.
pub const TOOL_SERVERS_KEY: &str = "tool_servers";
/// Key of the program and arguments inside a server table.
pub const COMMAND_KEY: &str = "command";
/// Key of the environment variable names inside a server table.
pub const ENV_KEY: &str = "env";
/// Key of the time limit inside a server table.
pub const SERVER_TIMEOUT_MS_KEY: &str = "timeout_ms";

/// Milliseconds one request to a server may take when `timeout_ms` is unset.
pub const DEFAULT_SERVER_TIMEOUT_MS: u64 = 30_000;
/// The most `timeout_ms` may be.
pub const MAX_SERVER_TIMEOUT_MS: u64 = 300_000;
/// Most servers that may be listed.
pub const MAX_SERVERS: usize = 8;
/// Most environment variable names one server may be given.
pub const MAX_ENV_NAMES: usize = 32;
/// Most bytes the program and arguments may hold together.
pub const MAX_COMMAND_BYTES: usize = 4096;
/// Longest server name, in bytes.
pub const MAX_NAME_BYTES: usize = 32;

/// One `[tool_servers.<name>]` section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolServerConfig {
    /// The program, as an absolute path, then its arguments. It is started with
    /// no shell and with the session's folder as its working directory.
    pub command: Vec<String>,
    /// Names of the environment variables the server is given from the daemon's
    /// own environment. It sees no other variable, `PATH` included.
    #[serde(default)]
    pub env: Vec<String>,
    /// The longest one request to the server may take, in milliseconds.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_SERVER_TIMEOUT_MS
}

/// Why a server name is refused, or `None` when it is 1 to 32 lowercase letters,
/// digits and hyphens that starts with a letter. No underscore is allowed, so
/// the `__` between a server name and a tool name is never ambiguous.
#[must_use]
pub fn name_problem(name: &str) -> Option<&'static str> {
    let plain = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if name.is_empty() || name.len() > MAX_NAME_BYTES || !plain {
        return Some("1 to 32 lowercase letters, digits and hyphens");
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Some("a name that starts with a letter");
    }
    None
}

/// Why an environment variable name is refused, or `None` when it is letters,
/// digits and underscores and does not start with a digit.
#[must_use]
pub fn env_name_problem(name: &str) -> Option<&'static str> {
    let plain = name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if name.is_empty() || !plain || name.starts_with(|c: char| c.is_ascii_digit()) {
        return Some("letters, digits and underscores, not starting with a digit");
    }
    None
}

impl ToolServerConfig {
    /// Why this section is refused, or `None` when it can be started.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        let Some(program) = self.command.first() else {
            return Some("a program and its arguments".to_owned());
        };
        if !Path::new(program).is_absolute() {
            return Some(format!(
                "an absolute path to the program first, not `{program}`"
            ));
        }
        if self.command.iter().map(String::len).sum::<usize>() > MAX_COMMAND_BYTES {
            return Some(format!("at most {MAX_COMMAND_BYTES} bytes in all"));
        }
        if self.command.iter().any(|part| part.contains('\0')) {
            return Some("no NUL byte in the program or an argument".to_owned());
        }
        None
    }
}
