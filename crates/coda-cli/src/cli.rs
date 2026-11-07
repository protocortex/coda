// SPDX-License-Identifier: Apache-2.0

//! The command line grammar.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use coda_protocol::types::{BackendKind, Mode};

const DEFAULT_TOOL: &str = "bash";

/// Coda: a coding agent daemon and its command line.
#[derive(Debug, Parser)]
#[command(name = "coda", version)]
pub struct Cli {
    /// Restart a daemon that is busy without asking.
    #[arg(long, global = true)]
    pub force: bool,
    /// What to do; without one, open the TUI on a terminal, else make sure
    /// the daemon is running.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon in the foreground, or manage the running one.
    Daemon(DaemonArgs),
    /// Open the terminal UI, starting the daemon if needed.
    Tui,
    /// Write a starter config.toml; an existing one is never overwritten.
    Init,
    /// Show the running daemon's state.
    Status,
    /// Check that a daemon answers.
    Ping,
    /// Run one prompt to completion.
    ///
    /// Exit codes: 0 turn ended, 1 turn limit, 2 budget used up, 3 context
    /// window full, 4 cancelled, 5 any other stop or a failed turn, 64 bad
    /// usage, 70 the run itself failed.
    Run(RunArgs),
    /// Review a change and print the findings; nothing is posted or edited.
    ///
    /// Exit codes: 0 reviewed or skipped by a rule, 64 bad usage, 70 the review
    /// failed.
    Review(ReviewArgs),
    /// Write or remove the git hooks that start a review.
    #[command(subcommand)]
    Hooks(HooksCommand),
    /// List, message and read stored sessions from a script.
    ///
    /// `send --wait` uses the exit codes of `coda run`; the other commands exit
    /// 0, 64 for bad usage, or 70 when they fail.
    #[command(subcommand)]
    Session(SessionCommand),
    /// Report and clean up the worktrees coda made for sessions.
    #[command(subcommand)]
    Worktree(WorktreeCommand),
    /// Manage standing permission grants.
    #[command(subcommand)]
    Grant(GrantCommand),
    /// Show token usage and savings.
    Usage(UsageArgs),
    /// Check the socket, database, backends and config, and repair leftovers.
    ///
    /// Exit codes: 0 healthy, 20 at least one check failed, 21 only warnings,
    /// 64 bad usage, 70 the check itself failed. They stay clear of the codes
    /// 0 to 5 that `run` uses.
    Doctor(DoctorArgs),
    /// Check, list and trust agent, skill and command definitions.
    #[command(subcommand)]
    Agents(AgentsCommand),
}

#[derive(Debug, Subcommand)]
pub enum HooksCommand {
    /// Write the hook. It reviews nothing until `[review.hook] enabled = true` is set.
    Install(HookArgs),
    /// Delete the hook, when coda wrote it.
    Remove(HookArgs),
}

#[derive(Debug, Args)]
pub struct HookArgs {
    /// Which hook.
    #[arg(value_enum)]
    pub name: HookName,
}

/// The hooks coda can write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookName {
    /// A pre-push hook that reviews the commits about to be pushed.
    Review,
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// List stored sessions, newest first.
    List(SessionListArgs),
    /// Send a message to a session; with --wait, follow the turn to its end.
    Send(SessionSendArgs),
    /// Print a session's conversation.
    Show(SessionShowArgs),
    /// Put the files back as an earlier turn left them and roll the later turns back.
    Rewind(SessionRewindArgs),
}

#[derive(Debug, Subcommand)]
pub enum WorktreeCommand {
    /// List the worktrees coda made and what cleanup would do with each; with
    /// --apply, remove the ones that are no longer needed.
    Clean(WorktreeCleanArgs),
}

#[derive(Debug, Args)]
pub struct WorktreeCleanArgs {
    /// Remove what the report marks for removal. Needs `[worktree.cleanup]
    /// enabled = true`. Without it nothing changes.
    #[arg(long)]
    pub apply: bool,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SessionRewindArgs {
    /// The session to rewind.
    pub session: String,
    /// The turn to go back to, or `base` for before the first turn.
    #[arg(long)]
    pub to: String,
    /// Leave the files as they are and roll back the conversation only.
    #[arg(long, conflicts_with = "keep_conversation")]
    pub keep_files: bool,
    /// Restore the files only and keep every turn in the conversation.
    #[arg(long)]
    pub keep_conversation: bool,
    /// Print the result as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SessionListArgs {
    /// Only sessions started in this directory.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Most sessions to list.
    #[arg(long)]
    pub limit: Option<u32>,
    /// Print the sessions as a JSON array.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct SessionSendArgs {
    /// The session to message.
    pub session: String,
    /// The message. Without it, standard input is the message.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Wait for the turn to end and exit with the code `coda run` would.
    #[arg(long)]
    pub wait: bool,
    /// Print a JSON object instead of text.
    #[arg(long)]
    pub json: bool,
    /// Maximum agent turns.
    #[arg(long)]
    pub max_turns: Option<u32>,
    /// Maximum tokens for the turn.
    #[arg(long)]
    pub token_cap: Option<u64>,
}

#[derive(Debug, Args)]
pub struct SessionShowArgs {
    /// The session to read.
    pub session: String,
    /// Print the session and its stored events as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum AgentsCommand {
    /// Check the definition files; exits 22 when any has a problem.
    Lint,
    /// List the usable definitions and the project files waiting for trust.
    List,
    /// Trust the project definitions of the current repository.
    Trust(TrustArgs),
}

#[derive(Debug, Args)]
pub struct TrustArgs {
    /// Stop trusting them instead.
    #[arg(long)]
    pub revoke: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Remove a stale socket and kill orphaned backends; a running
    /// daemon is never touched.
    #[arg(long)]
    pub repair: bool,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct UsageArgs {
    /// Only this session; defaults to every session.
    #[arg(long)]
    pub session: Option<String>,
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub action: Option<DaemonAction>,
}

#[derive(Debug, Subcommand)]
pub enum DaemonAction {
    /// Replace the running daemon with a fresh one.
    Restart,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// The prompt to run.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Run the command file with this name; its prompt template is the prompt.
    #[arg(long, conflicts_with = "prompt")]
    pub command: Option<String>,
    /// The arguments of --command, placed where its template has `$ARGUMENTS`.
    /// Without this, piped standard input is the arguments.
    #[arg(long, requires = "command", allow_hyphen_values = true)]
    pub args: Option<String>,
    /// Working directory of the run.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Work in a new git worktree on its own branch, `coda/<session>`, so the
    /// run does not touch your checkout. Configure it under `[worktree]`.
    #[arg(long)]
    pub worktree: bool,
    /// How much autonomy the agent has.
    #[arg(long, value_enum, default_value_t)]
    pub mode: ModeArg,
    /// Maximum agent turns.
    #[arg(long)]
    pub max_turns: Option<u32>,
    /// Maximum tokens for the run.
    #[arg(long)]
    pub token_cap: Option<u64>,
}

#[derive(Debug, Args)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is its own option on the command line"
)]
pub struct ReviewArgs {
    /// A pull request number or URL, a branch, or a range such as `main...topic`.
    /// Without one, the current branch is reviewed against the default branch.
    pub target: Option<String>,
    /// Review the staged changes instead.
    #[arg(long, conflicts_with = "target")]
    pub staged: bool,
    /// Extra instructions for the reviewer.
    #[arg(long)]
    pub instructions: Option<String>,
    /// Print the review as JSON instead of markdown.
    #[arg(long)]
    pub json: bool,
    /// The backend that runs the reviewer. By default the one `coda run` uses.
    #[arg(long, value_enum)]
    pub backend: Option<BackendArg>,
    /// Working directory of the review.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Post the findings to the pull request as a pending review plus a summary
    /// comment, after a preview you confirm. Needs a pull request target; it never
    /// approves or requests changes.
    #[arg(long, conflicts_with = "staged")]
    pub post: bool,
    /// Post without asking. Needed when standard input is not a terminal.
    #[arg(long, requires = "post")]
    pub yes: bool,
    /// Review only what came after the last review of this pull request, and do nothing
    /// when too little is new. Needs a pull request target.
    #[arg(long, conflicts_with = "staged")]
    pub incremental: bool,
    /// What the pre-push hook runs: review the commits not yet pushed (`@{u}..HEAD`), print
    /// the findings, and exit 23 when a finding of severity high or critical remains and
    /// `[review.hook] block_on_high` is set. Does nothing unless `[review.hook] enabled` is set.
    #[arg(long, conflicts_with_all = ["target", "staged", "post", "incremental", "dismiss", "context_file", "context_note", "run_checks"])]
    pub hook: bool,
    /// Dismiss the finding of this pull request that starts with this fingerprint, so later
    /// reviews leave it out, and stop. May be given more than once.
    #[arg(long, value_name = "FINGERPRINT", conflicts_with_all = ["staged", "post", "incremental"])]
    pub dismiss: Vec<String>,
    /// Add a file of test, coverage or linter output to the prompt, as untrusted data. May
    /// be given up to five times; each file is cut at 20000 characters.
    #[arg(long, value_name = "PATH")]
    pub context_file: Vec<PathBuf>,
    /// One short line on how the reviewer should use the context files or the check output.
    #[arg(long, value_name = "TEXT")]
    pub context_note: Option<String>,
    /// Run the `[review] check_commands` through the `tester` agent first and add what it
    /// reports to the prompt, as untrusted data. Needs a model-API backend.
    #[arg(long)]
    pub run_checks: bool,
}

/// The backends as typed on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackendArg {
    Anthropic,
    ClaudeCode,
    Codex,
}

impl From<BackendArg> for BackendKind {
    fn from(arg: BackendArg) -> Self {
        match arg {
            BackendArg::Anthropic => Self::Anthropic,
            BackendArg::ClaudeCode => Self::ClaudeCode,
            BackendArg::Codex => Self::CodexAppServer,
        }
    }
}

/// The autonomy modes as typed on the command line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    #[default]
    Default,
    Plan,
    AcceptEdits,
}

impl From<ModeArg> for Mode {
    fn from(arg: ModeArg) -> Self {
        match arg {
            ModeArg::Default => Self::Default,
            ModeArg::Plan => Self::Plan,
            ModeArg::AcceptEdits => Self::AcceptEdits,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum GrantCommand {
    /// Always allow one command in a repository.
    Add(GrantAddArgs),
}

#[derive(Debug, Args)]
pub struct GrantAddArgs {
    /// A directory inside the repository the grant belongs to.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Directory the command must run in; defaults to the current one.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Tool the grant covers.
    #[arg(long, default_value = DEFAULT_TOOL)]
    pub tool: String,
    /// The exact command to allow.
    pub command: String,
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn run_flags_parse_into_typed_values() {
        // Arrange
        let args = [
            "coda",
            "run",
            "--prompt",
            "hi",
            "--cwd",
            "/x",
            "--mode",
            "accept-edits",
            "--max-turns",
            "3",
            "--token-cap",
            "100",
        ];

        // Act
        let cli = Cli::try_parse_from(args).unwrap();

        // Assert
        let Some(Command::Run(run)) = cli.command else {
            panic!("expected the run command");
        };
        assert_eq!(run.prompt.as_deref(), Some("hi"));
        assert_eq!(run.cwd, Some(PathBuf::from("/x")));
        assert_eq!(run.mode, ModeArg::AcceptEdits);
        assert_eq!((run.max_turns, run.token_cap), (Some(3), Some(100)));
    }

    #[test]
    fn a_command_takes_arguments_that_may_start_with_a_dash() {
        // Arrange
        let args = ["coda", "run", "--command", "review", "--args", "--fast 42"];

        // Act
        let cli = Cli::try_parse_from(args).unwrap();

        // Assert
        let Some(Command::Run(run)) = cli.command else {
            panic!("expected the run command");
        };
        assert_eq!(run.command.as_deref(), Some("review"));
        assert_eq!(run.args.as_deref(), Some("--fast 42"));
    }

    #[test]
    fn a_command_excludes_a_prompt_and_arguments_need_a_command() {
        for args in [
            ["coda", "run", "--command", "review", "--prompt", "x"].as_slice(),
            ["coda", "run", "--args", "x"].as_slice(),
        ] {
            // Act
            let parsed = Cli::try_parse_from(args);

            // Assert
            assert!(parsed.is_err(), "{args:?}");
        }
    }

    #[test]
    fn review_takes_a_target_or_staged_but_not_both() {
        // Arrange
        let both = ["coda", "review", "--staged", "main"];
        let target = [
            "coda",
            "review",
            "main...topic",
            "--json",
            "--backend",
            "codex",
        ];

        // Act
        let refused = Cli::try_parse_from(both);
        let parsed = Cli::try_parse_from(target).unwrap();

        // Assert
        assert!(refused.is_err());
        let Some(Command::Review(review)) = parsed.command else {
            panic!("expected the review command");
        };
        assert_eq!(review.target.as_deref(), Some("main...topic"));
        assert!(review.json);
        assert_eq!(review.backend, Some(BackendArg::Codex));
    }

    #[test]
    fn session_commands_parse_their_flags() {
        // Arrange
        let list = [
            "coda", "session", "list", "--cwd", "/x", "--limit", "5", "--json",
        ];
        let send = ["coda", "session", "send", "s-1", "--prompt", "hi", "--wait"];
        let show = ["coda", "session", "show", "s-1", "--json"];

        // Act
        let list = Cli::try_parse_from(list).unwrap();
        let send = Cli::try_parse_from(send).unwrap();
        let show = Cli::try_parse_from(show).unwrap();

        // Assert
        let Some(Command::Session(SessionCommand::List(list))) = list.command else {
            panic!("expected session list");
        };
        assert_eq!(
            (list.cwd, list.limit, list.json),
            (Some(PathBuf::from("/x")), Some(5), true)
        );
        let Some(Command::Session(SessionCommand::Send(send))) = send.command else {
            panic!("expected session send");
        };
        assert_eq!(
            (send.session.as_str(), send.prompt.as_deref(), send.wait),
            ("s-1", Some("hi"), true)
        );
        let Some(Command::Session(SessionCommand::Show(show))) = show.command else {
            panic!("expected session show");
        };
        assert!(show.json);
    }

    #[test]
    fn every_mode_argument_maps_to_the_protocol_mode() {
        for (arg, mode) in [
            (ModeArg::Default, Mode::Default),
            (ModeArg::Plan, Mode::Plan),
            (ModeArg::AcceptEdits, Mode::AcceptEdits),
        ] {
            // Act and Assert
            assert_eq!(Mode::from(arg), mode);
        }
    }
}
