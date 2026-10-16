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
    #[error("no daemon is running; run `coda` to start one")]
    NotRunning,
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
    #[error("`coda run` is not yet available")]
    RunUnavailable,
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
    #[error("could not write output: {0}")]
    Output(#[from] io::Error),
}
