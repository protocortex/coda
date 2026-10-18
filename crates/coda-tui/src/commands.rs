// SPDX-License-Identifier: AGPL-3.0-only

//! Slash commands typed into the prompt.

use coda_protocol::types::SessionId;

const SLASH: char = '/';
const COMMANDS: [&str; 6] = ["sessions", "new", "resume", "model", "backend", "quit"];

/// A command the user can type instead of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// List the sessions this client knows.
    Sessions,
    /// Start a fresh session.
    New,
    /// Reopen a session by id.
    Resume(SessionId),
    /// Switch the current session to a model alias.
    Model(String),
    /// List the backends, or start a new session on the named one.
    Backend(Option<String>),
    Quit,
}

/// Why a line that starts with a slash is not a command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error(
        "unknown command {0}; try /sessions, /new, /resume <id>, /model <alias>, /backend [name] or /quit"
    )]
    Unknown(String),
    #[error("/resume needs a session id; list them with /sessions")]
    MissingSessionId,
    #[error("/model needs a model alias; define aliases under [models.<alias>] in the config")]
    MissingModel,
    #[error("{0} takes a single argument; remove the extra words")]
    ExtraArguments(String),
}

/// Reads `line` as a command; `Ok(None)` means it is an ordinary message.
///
/// A first word is a command name only when it is a slash followed by a name
/// without further slashes, so a prompt that opens with a path such as
/// `/usr/bin` or with `//` is a message. A path whose first segment is a command
/// name, such as `/quit/`, is a mistyped command.
///
/// # Errors
/// Returns [`CommandError`] when the line starts with a command-shaped word that names
/// no command, lacks an argument or carries extra words, so a mistyped command is
/// never sent to the model.
pub fn parse(line: &str) -> Result<Option<Command>, CommandError> {
    let mut words = line.split_whitespace();
    let Some(word) = words.next() else {
        return Ok(None);
    };
    let Some(rest) = word.strip_prefix(SLASH) else {
        return Ok(None);
    };
    let name = rest.split(SLASH).next().unwrap_or_default();
    if name.is_empty() {
        return Ok(None);
    }
    if rest.contains(SLASH) {
        return if COMMANDS.contains(&name) {
            Err(CommandError::Unknown(word.to_owned()))
        } else {
            Ok(None)
        };
    }
    let command = match name {
        "sessions" => Command::Sessions,
        "new" => Command::New,
        "quit" => Command::Quit,
        "resume" => Command::Resume(SessionId::new(
            words.next().ok_or(CommandError::MissingSessionId)?,
        )),
        "model" => Command::Model(words.next().ok_or(CommandError::MissingModel)?.to_owned()),
        "backend" => Command::Backend(words.next().map(str::to_owned)),
        _ => return Err(CommandError::Unknown(word.to_owned())),
    };
    match (&command, words.next()) {
        (Command::Model(_) | Command::Backend(_), Some(_)) => {
            Err(CommandError::ExtraArguments(word.to_owned()))
        }
        _ => Ok(Some(command)),
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
            ("/bogus", Err(CommandError::Unknown("/bogus".to_owned()))),
            ("/resume", Err(CommandError::MissingSessionId)),
        ] {
            // Arrange / Act
            let parsed = parse(line);

            // Assert
            assert_eq!(parsed, expected, "{line}");
        }
    }

    #[test]
    fn model_and_backend_commands_take_their_argument() {
        for (line, expected) in [
            ("/model fast", Ok(Some(Command::Model("fast".to_owned())))),
            (
                "  /model   fast  extra ",
                Err(CommandError::ExtraArguments("/model".to_owned())),
            ),
            (
                "/backend faux extra",
                Err(CommandError::ExtraArguments("/backend".to_owned())),
            ),
            ("/model", Err(CommandError::MissingModel)),
            ("/model   ", Err(CommandError::MissingModel)),
            ("/backend", Ok(Some(Command::Backend(None)))),
            (
                "/backend faux",
                Ok(Some(Command::Backend(Some("faux".to_owned())))),
            ),
        ] {
            // Arrange / Act
            let parsed = parse(line);

            // Assert
            assert_eq!(parsed, expected, "{line}");
        }
    }

    #[test]
    fn a_prompt_that_starts_with_a_path_or_slashes_is_a_message() {
        for line in [
            "/usr/bin",
            "/usr/bin is on the path",
            "//",
            "// comment",
            "/",
            "/ alone",
            "",
            "   ",
        ] {
            // Arrange / Act
            let parsed = parse(line);

            // Assert
            assert_eq!(parsed, Ok(None), "{line:?}");
        }
    }

    #[test]
    fn a_command_name_with_a_trailing_slash_is_a_mistyped_command() {
        for line in ["/quit/", "/new/", "/model/ fast", "/backend/x"] {
            // Arrange / Act
            let parsed = parse(line);

            // Assert
            let word = line.split_whitespace().next().unwrap().to_owned();
            assert_eq!(parsed, Err(CommandError::Unknown(word)), "{line}");
        }
    }

    #[test]
    fn a_path_whose_first_segment_is_not_a_command_stays_a_message() {
        for line in ["/usr/bin", "/newton/laws", "/quitters/"] {
            // Arrange / Act
            let parsed = parse(line);

            // Assert
            assert_eq!(parsed, Ok(None), "{line}");
        }
    }
}
