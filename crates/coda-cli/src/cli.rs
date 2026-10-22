// SPDX-License-Identifier: AGPL-3.0-only

//! The command line grammar.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use coda_protocol::types::Mode;

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
pub enum AgentsCommand {
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
    /// Working directory of the run.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
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
