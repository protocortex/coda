// SPDX-License-Identifier: AGPL-3.0-only

//! Finds the running daemon, asks about it and stops it.

use std::io;

use coda_client::{Client, ClientError};
use coda_protocol::methods::{
    DAEMON_SHUTDOWN, DAEMON_STATUS, DaemonShutdownParams, DaemonShutdownResult, DaemonStatusParams,
    DaemonStatusResult,
};
use tokio::time::Instant;
use tracing::{debug, info};

use crate::context::Context;
use crate::error::CliError;

/// The status request itself keeps one connection open.
const OWN_CONNECTION: u32 = 1;

/// What a daemon is busy with, not counting the asking CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activity {
    pub active_turns: u32,
    pub other_clients: u32,
}

impl Activity {
    #[must_use]
    pub fn of(status: &DaemonStatusResult) -> Self {
        Self {
            active_turns: status.active_turns,
            other_clients: status.client_count.saturating_sub(OWN_CONNECTION),
        }
    }

    #[must_use]
    pub fn is_idle(self) -> bool {
        self.active_turns == 0 && self.other_clients == 0
    }
}

/// Words for what a daemon is busy with, or that it could not be asked.
#[must_use]
pub fn describe(activity: Option<Activity>) -> String {
    match activity {
        Some(a) => format!(
            "{} active turn(s) and {} attached client(s)",
            a.active_turns, a.other_clients
        ),
        None => "unknown activity".to_owned(),
    }
}

fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

/// Connects without a handshake, so any daemon version can be asked.
///
/// # Errors
/// Returns [`CliError::NotRunning`] when nothing listens on the socket.
pub async fn connect_unchecked(ctx: &Context) -> Result<Client, CliError> {
    match Client::connect_unchecked(&ctx.dirs.socket_path(), ctx.timing).await {
        Ok(client) => Ok(client),
        Err(ClientError::Connect { source, .. }) if is_absent(&source) => Err(CliError::NotRunning),
        Err(e) => Err(e.into()),
    }
}

/// Asks the daemon for its state.
///
/// # Errors
/// Returns the client error of the `daemon.status` call.
pub async fn status_of(client: &Client) -> Result<DaemonStatusResult, CliError> {
    Ok(client
        .request(DAEMON_STATUS, &DaemonStatusParams {})
        .await?)
}

/// Returns a client for the running daemon.
///
/// # Errors
/// Returns [`CliError::NotRunning`] when nothing listens on the socket.
pub async fn ensure_daemon(ctx: &Context) -> Result<Client, CliError> {
    match Client::connect(&ctx.dirs.socket_path(), ctx.timing).await {
        Ok(client) => Ok(client),
        Err(ClientError::Connect { source, .. }) if is_absent(&source) => Err(CliError::NotRunning),
        Err(e) => Err(e.into()),
    }
}

/// Asks the daemon behind `client` to shut down and waits until its socket is gone.
///
/// # Errors
/// Returns [`CliError::StopTimeout`] if the daemon keeps answering, or a client error
/// other than the connection closing during the shutdown itself.
pub async fn stop_daemon(ctx: &Context, client: Client) -> Result<(), CliError> {
    info!("asking the daemon to shut down");
    let reply: Result<DaemonShutdownResult, ClientError> = client
        .request(DAEMON_SHUTDOWN, &DaemonShutdownParams {})
        .await;
    match reply {
        // The daemon may close the connection before the reply arrives.
        Ok(_) | Err(ClientError::Disconnected) => {}
        Err(e) => return Err(e.into()),
    }
    drop(client);
    let started = Instant::now();
    loop {
        match connect_unchecked(ctx).await {
            Err(CliError::NotRunning) => return Ok(()),
            Err(e) => return Err(e),
            Ok(_) => debug!("the daemon still answers"),
        }
        if started.elapsed() >= ctx.wait.timeout {
            return Err(CliError::StopTimeout {
                waited: ctx.wait.timeout,
            });
        }
        tokio::time::sleep(ctx.wait.poll).await;
    }
}
