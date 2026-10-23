// SPDX-License-Identifier: AGPL-3.0-only

//! Slash commands typed into the prompt.

use std::collections::BTreeMap;

use coda_agents::{Catalog, CommandDef, CommandLookupError, Kind, Source};
use coda_protocol::types::SessionId;

use crate::permission::sanitize;

const SLASH: char = '/';
/// The commands the UI itself runs. They win over a command file of the same name.
const COMMANDS: [&str; 7] = [
    "sessions", "new", "resume", "model", "backend", "help", "quit",
];
/// The built-in commands as `/help` lists them.
const BUILT_IN_HELP: &str = "Built-in commands: /sessions, /new, /resume <id>, /model <alias>, /backend [name], /help, /quit";

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
    /// List the built-in commands and the ones from command files.
    Help,
    Quit,
}

/// Why a line that starts with a slash is not a command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandError {
    #[error("unknown command {0}; type /help to list the commands")]
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
        "help" => Command::Help,
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

/// A command file that can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCommand {
    pub def: CommandDef,
    pub source: Source,
}

/// What typing a word that is not a built-in command comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// The prompt to send in place of the typed line.
    Prompt(String),
    /// The command exists but cannot run; the reason is for the user.
    Refused(String),
    /// No command file has that name.
    Unknown,
}

/// Ends the advice on a pending command, since the UI reads the command files
/// and the trust decision once, when it starts.
const RESTART_NOTE: &str = "then restart the UI";

/// The commands from command files, as the UI offers them.
///
/// Only user commands and project commands the user trusted can run. A project
/// command that waits for trust is listed as pending and never runs. A command
/// whose name is a built-in is ignored, because the built-in wins. What a name
/// comes to is decided once, by [`Catalog::runnable_command`], when the UI
/// starts, so a later `coda agents trust` or `--revoke` reaches the UI only
/// after a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserCommands {
    /// Each command file name that is not a built-in: the command, or why it
    /// cannot run.
    named: BTreeMap<String, Result<UserCommand, CommandLookupError>>,
    /// Commands hidden by a built-in of the same name.
    shadowed: Vec<(String, Source)>,
    /// Says that definition files were skipped, when some were.
    skipped: Option<String>,
    /// Names of user commands that a project file waiting for trust would replace.
    replaceable: Vec<String>,
    /// Why the command files could not be read, when they could not.
    warning: Option<String>,
}

impl UserCommands {
    /// No command files, because reading them failed for `reason`. The UI
    /// starts with its built-ins and shows `reason` as a warning.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            warning: Some(reason.into()),
            ..Self::default()
        }
    }

    /// Takes the commands out of `catalog`.
    #[must_use]
    pub fn from_catalog(catalog: &Catalog) -> Self {
        let mut commands = Self {
            skipped: catalog.problems_hint(),
            ..Self::default()
        };
        let loaded = catalog
            .commands()
            .map(|entry| (entry.def.name.clone(), entry.source));
        let waiting: Vec<&str> = catalog
            .pending()
            .iter()
            .filter(|pending| pending.kind == Kind::Command)
            .filter_map(|pending| pending.name())
            .collect();
        // A trusted project file replaces a user command of its name, so the
        // user command runs only until the user trusts the project.
        commands.replaceable = waiting
            .iter()
            .filter(|name| !COMMANDS.contains(name) && catalog.command(name).is_some())
            .map(|name| (*name).to_owned())
            .collect();
        let waiting = waiting
            .into_iter()
            .map(|name| (name.to_owned(), Source::Project));
        for (name, source) in loaded.chain(waiting) {
            // Trusting a waiting file would not make it run, so it is a clash, not a wait.
            if COMMANDS.contains(&name.as_str()) {
                commands.shadowed.push((name, source));
                continue;
            }
            match catalog.runnable_command(&name) {
                Ok(entry) => {
                    let command = UserCommand {
                        def: entry.def.clone(),
                        source: entry.source,
                    };
                    commands.named.insert(name, Ok(command));
                }
                Err(refused @ CommandLookupError::Pending { .. }) => {
                    commands.named.insert(name, Err(refused));
                }
                Err(CommandLookupError::Unknown { .. }) => {}
            }
        }
        commands
    }

    /// The first word of `line` when it is a command file's name followed by
    /// nothing but slashes, such as `/deploy/`. [`parse`] leaves a path alone,
    /// and so does this when something follows the slash (`/deploy/staging`),
    /// since that is a path in a message.
    #[must_use]
    pub fn mistyped(&self, line: &str) -> Option<String> {
        let word = line.split_whitespace().next()?;
        let (name, tail) = word.strip_prefix(SLASH)?.split_once(SLASH)?;
        let known = self.named.contains_key(name);
        (known && tail.chars().all(|c| c == SLASH)).then(|| word.to_owned())
    }

    /// One line per command file hidden by a built-in, for the user to read.
    #[must_use]
    pub fn clash_notices(&self) -> Vec<String> {
        self.shadowed
            .iter()
            .map(|(name, source)| {
                format!(
                    "the {} command file {SLASH}{name} is ignored: {SLASH}{name} is a built-in command",
                    source.as_str()
                )
            })
            .collect()
    }

    /// What to tell the user when the UI opens: a warning that the command
    /// files could not be read, that some definition files were skipped, each
    /// project file that waits to replace a user command, and each file a
    /// built-in hides.
    #[must_use]
    pub fn startup_notices(&self) -> Vec<String> {
        let warning = self.warning.iter().map(|text| sanitize(text).0);
        let replaceable = self.replaceable.iter().map(|name| {
            let name = sanitize(name).0;
            format!(
                "a project file for {SLASH}{name} waits for trust; after `coda agents trust` and a restart it replaces your user command {SLASH}{name}"
            )
        });
        warning
            .chain(self.skipped.clone())
            .chain(replaceable)
            .chain(self.clash_notices())
            .collect()
    }

    /// The lines `/help` shows.
    #[must_use]
    pub fn help_lines(&self) -> Vec<String> {
        let mut lines = vec![BUILT_IN_HELP.to_owned()];
        let runnable = self
            .named
            .iter()
            .filter_map(|(name, lookup)| lookup.as_ref().ok().map(|command| (name, command)));
        for (name, command) in runnable {
            let hint = command
                .def
                .argument_hint
                .as_deref()
                .map(|hint| format!(" {}", sanitize(hint).0))
                .unwrap_or_default();
            lines.push(format!(
                "{SLASH}{name}{hint}: {} ({})",
                sanitize(&command.def.description).0,
                command.source.as_str()
            ));
        }
        let waiting = self.named.iter().filter_map(|(name, lookup)| match lookup {
            Err(CommandLookupError::Pending { reason, .. }) => Some((name, reason)),
            _ => None,
        });
        for (name, reason) in waiting {
            lines.push(format!(
                "{SLASH}{}: project command, pending ({}); review the files, then run `coda agents trust`, {RESTART_NOTE}",
                sanitize(name).0,
                reason.as_str()
            ));
        }
        lines.extend(self.startup_notices());
        lines
    }

    /// Reads the typed word (with its slash) and arguments as a command file.
    #[must_use]
    pub fn resolve(&self, word: &str, args: &str) -> Resolved {
        let Some(name) = word.strip_prefix(SLASH) else {
            return Resolved::Unknown;
        };
        match self.named.get(name) {
            Some(Ok(command)) => match command.def.expand(args) {
                Ok(prompt) => Resolved::Prompt(prompt),
                Err(error) => Resolved::Refused(format!("{word}: {error}")),
            },
            Some(Err(refused)) => {
                Resolved::Refused(sanitize(&format!("{SLASH}{refused}, {RESTART_NOTE}")).0)
            }
            None => Resolved::Unknown,
        }
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
            ("/help", Ok(Some(Command::Help))),
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
    fn every_built_in_is_in_help_and_parses_as_a_command() {
        let help = UserCommands::default().help_lines().join("\n");
        for name in COMMANDS {
            // Arrange / Act
            let parsed = parse(&format!("{SLASH}{name} x"));

            // Assert
            assert!(
                help.contains(&format!("{SLASH}{name}")),
                "/help does not list {name}: {help}"
            );
            assert!(
                matches!(parsed, Ok(Some(_)))
                    || matches!(parsed, Err(CommandError::ExtraArguments(_))),
                "{name} is in COMMANDS but parse does not know it: {parsed:?}"
            );
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
