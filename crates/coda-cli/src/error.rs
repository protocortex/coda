// SPDX-License-Identifier: AGPL-3.0-only

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use coda_client::ClientError;
use coda_config::config::ConfigError;
use coda_daemon::server::ServerError;
use coda_store::StoreError;
use coda_telemetry::init::TelemetryError;
use coda_tui::TuiError;

/// Why a `coda` command failed. The messages are shown to the user as they are.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("could not start telemetry: {0}")]
    Telemetry(#[from] TelemetryError),
    #[error("could not open the database: {0}")]
    Store(#[from] StoreError),
    #[error("could not start the daemon server: {0}")]
    Server(#[from] ServerError),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("the terminal UI failed: {0}")]
    Tui(#[from] TuiError),
    #[error("`coda tui` needs an interactive terminal; use `coda status` to see the daemon")]
    NoTerminal,
    #[error("{} already exists; edit it, or remove it to start over", path.display())]
    ConfigExists { path: PathBuf },
    #[error("no daemon is running; run `coda` to start one")]
    NotRunning,
    #[error("the running daemon is older than this coda; restart it with `coda daemon restart`")]
    DaemonTooOld,
    #[error("the daemon did not answer within {} ms", waited.as_millis())]
    StartTimeout { waited: Duration },
    #[error("the old daemon did not stop within {} ms", waited.as_millis())]
    StopTimeout { waited: Duration },
    #[error("could not start `{}`: {source}", exe.display())]
    Spawn { exe: PathBuf, source: io::Error },
    #[error("the daemon exited at startup ({status}); see {}", log.display())]
    DaemonExited { status: ExitStatus, log: PathBuf },
    #[error("cannot grant outside a git repository; you will be prompted each time")]
    NoGitRoot,
    #[error("no prompt: pass --prompt or --command, or write the prompt to standard input")]
    EmptyPrompt,
    #[error("no command named {name}; list the command files with `coda agents list`{}", lint_suffix(.lint_hint.as_deref()))]
    UnknownCommand {
        name: String,
        /// Says that definition files were skipped, when some were, since one
        /// of them may be the command that was asked for.
        lint_hint: Option<String>,
    },
    #[error(transparent)]
    CommandRefused(#[from] coda_agents::CommandLookupError),
    #[error(
        "standard input is more than {limit} bytes; it is the arguments of the command, so pass less, or use --args"
    )]
    CommandInputTooLarge { limit: usize },
    #[error("{name}: {source}")]
    CommandArguments {
        name: String,
        source: coda_agents::ExpandError,
    },
    #[error("the connection to the daemon was lost before the run ended")]
    RunInterrupted,
    #[error("the daemon has {activity}; use --force to restart it anyway")]
    DaemonBusy { activity: String },
    #[error("the running daemon was left alone")]
    RestartDeclined,
    #[error("cannot use {}: {source}", path.display())]
    Path { path: PathBuf, source: io::Error },
    #[error("could not read the process environment: {0}")]
    Environment(io::Error),
    #[error("could not wait for a shutdown signal: {0}")]
    Signal(io::Error),
    #[error("could not repair: {0}")]
    Repair(#[from] coda_daemon::repair::RepairError),
    #[error("could not encode the report: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not write output: {0}")]
    Output(#[from] io::Error),
}

/// What follows an unknown-command message: the lint hint, if there is one.
fn lint_suffix(hint: Option<&str>) -> String {
    hint.map(|hint| format!("; {hint}")).unwrap_or_default()
}
