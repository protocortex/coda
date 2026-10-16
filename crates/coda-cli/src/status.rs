// SPDX-License-Identifier: AGPL-3.0-only

//! `coda status`, `coda ping` and the bare `coda` start-up report.

use coda_protocol::methods::{DAEMON_PING, PingParams, PingResult};

use crate::autostart::{Activity, connect_unchecked, ensure_daemon, status_of};
use crate::context::{Context, Io};
use crate::error::CliError;

/// Prints the state of the running daemon.
///
/// # Errors
/// Returns [`CliError::NotRunning`] when no daemon answers.
pub async fn show(ctx: &Context, io: &mut Io<'_>) -> Result<(), CliError> {
    let client = connect_unchecked(ctx).await?;
    let status = status_of(&client).await?;
    let activity = Activity::of(&status);
    writeln!(io.out, "daemon is running")?;
    writeln!(io.out, "pid: {}", status.pid)?;
    writeln!(io.out, "version: {}", status.version)?;
    writeln!(io.out, "protocol: {}", status.protocol_version)?;
    writeln!(io.out, "uptime: {}s", status.uptime_secs)?;
    writeln!(io.out, "attached clients: {}", activity.other_clients)?;
    writeln!(io.out, "active turns: {}", activity.active_turns)?;
    writeln!(io.out, "data dir: {}", status.data_dir)?;
    Ok(())
}

/// Checks that a daemon answers `daemon.ping`.
///
/// # Errors
/// Returns [`CliError::NotRunning`] when no daemon answers.
pub async fn ping(ctx: &Context, io: &mut Io<'_>) -> Result<(), CliError> {
    let client = connect_unchecked(ctx).await?;
    let _: PingResult = client.request(DAEMON_PING, &PingParams {}).await?;
    writeln!(io.out, "pong")?;
    Ok(())
}

/// Makes sure a daemon is running, starting one when absent.
///
/// # Errors
/// Returns the errors of [`ensure_daemon`].
pub async fn start(ctx: &Context, io: &mut Io<'_>, force: bool) -> Result<(), CliError> {
    let client = ensure_daemon(ctx, io, force).await?;
    let status = status_of(&client).await?;
    writeln!(io.out, "daemon is running (pid {})", status.pid)?;
    Ok(())
}
