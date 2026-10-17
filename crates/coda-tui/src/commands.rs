// SPDX-License-Identifier: AGPL-3.0-only

//! Slash commands typed into the prompt.

use coda_protocol::types::SessionId;

const SLASH: char = '/';

/// A command the user can type instead of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// List the sessions this client knows.
    Sessions,
    /// Start a fresh session.
    New,
    /// Reopen a session by id.
    Resume(SessionId),
    Quit,
}

/// Why a line that starts with a slash is not a command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("unknown command {0}; try /sessions, /new, /resume <id> or /quit")]
    Unknown(String),
    #[error("/resume needs a session id; list them with /sessions")]
    MissingSessionId,
}

/// Reads `line` as a command; `Ok(None)` means it is an ordinary message.
///
/// # Errors
/// Returns [`CommandError`] when the line starts with a slash but names no command
/// or lacks an argument, so a mistyped command is never sent to the model.
pub fn parse(line: &str) -> Result<Option<Command>, CommandError> {
    let mut words = line.split_whitespace();
    let Some(name) = words.next().filter(|word| word.starts_with(SLASH)) else {
        return Ok(None);
    };
    match name {
        "/sessions" => Ok(Some(Command::Sessions)),
        "/new" => Ok(Some(Command::New)),
        "/quit" => Ok(Some(Command::Quit)),
        "/resume" => words
            .next()
            .map(|id| Some(Command::Resume(SessionId::new(id))))
            .ok_or(CommandError::MissingSessionId),
        other => Err(CommandError::Unknown(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse_text_is_a_message_and_a_mistyped_command_is_an_error() {
        for (line, expected) in [
            ("/sessions", Ok(Some(Command::Sessions))),
            ("  /new  ", Ok(Some(Command::New))),
            ("/quit", Ok(Some(Command::Quit))),
            (
                "/resume s-2",
                Ok(Some(Command::Resume(SessionId::new("s-2")))),
            ),
            ("what is 1/2", Ok(None)),
            (
                "/etc/passwd",
                Err(CommandError::Unknown("/etc/passwd".to_owned())),
            ),
            ("/resume", Err(CommandError::MissingSessionId)),
        ] {
            // Arrange / Act
            let parsed = parse(line);

            // Assert
            assert_eq!(parsed, expected, "{line}");
        }
    }
}
