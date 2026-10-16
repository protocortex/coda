// SPDX-License-Identifier: AGPL-3.0-only

//! Finds the daemon, starts it when absent and replaces one speaking another protocol.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

use coda_client::{Client, ClientError};
use coda_config::config::CODA_HOME_VAR;
use coda_daemon::dispatch::PROTOCOL_VERSION_MISMATCH;
use coda_protocol::methods::{
    DAEMON_SHUTDOWN, DAEMON_STATUS, DaemonShutdownParams, DaemonShutdownResult, DaemonStatusParams,
    DaemonStatusResult,
};
use tokio::time::Instant;
use tracing::{debug, info};

use crate::context::{Context, Io};
use crate::error::CliError;

/// File under the base directory that receives the stderr of a daemon started in the background.
pub const DAEMON_LOG_FILE: &str = "daemon.log";
const DAEMON_LOG_MODE: u32 = 0o600;
const DAEMON_SUBCOMMAND: &str = "daemon";
/// The status request itself keeps one connection open.
const OWN_CONNECTION: u32 = 1;
const YES_ANSWERS: [&str; 2] = ["y", "yes"];

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

/// What to do about a daemon that speaks another protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    Restart,
    AskUser,
}

/// Restarts at once only when nothing would be interrupted or the user forced it.
/// An unknown activity counts as busy.
#[must_use]
pub fn decide_restart(activity: Option<Activity>, force: bool) -> RestartDecision {
    if force || activity.is_some_and(Activity::is_idle) {
        RestartDecision::Restart
    } else {
        RestartDecision::AskUser
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

/// Asks whether to restart a busy daemon; anything but an explicit yes is no.
///
/// # Errors
/// Returns the I/O error of writing the prompt or reading the answer.
pub fn confirm_restart(io: &mut Io<'_>, activity: Option<Activity>) -> io::Result<bool> {
    let busy = describe(activity);
    write!(
        io.out,
        "The running daemon speaks another protocol version and has {busy}. Restart it? [y/N] "
    )?;
    io.out.flush()?;
    let mut answer = String::new();
    io.input.read_line(&mut answer)?;
    Ok(YES_ANSWERS.contains(&answer.trim().to_lowercase().as_str()))
}

enum Probe {
    Running(Client),
    Absent,
    Mismatch,
}

/// Whether no daemon can answer on the socket. Linux resets a connection to a
/// listener that closes while it is queued, so a reset means a daemon going away.
fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
    )
}

async fn probe(ctx: &Context) -> Result<Probe, CliError> {
    match Client::connect(&ctx.dirs.socket_path(), ctx.timing).await {
        Ok(client) => Ok(Probe::Running(client)),
        Err(ClientError::Connect { source, .. }) if is_absent(&source) => Ok(Probe::Absent),
        Err(ClientError::ProtocolMismatch { .. }) => Ok(Probe::Mismatch),
        Err(ClientError::Rpc { code, .. }) if code == PROTOCOL_VERSION_MISMATCH => {
            Ok(Probe::Mismatch)
        }
        Err(e) => Err(e.into()),
    }
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

/// Returns a client for a daemon that speaks this protocol, starting one when
/// absent and replacing one that speaks another version.
///
/// # Errors
/// Returns [`CliError::RestartDeclined`] if the user keeps a busy mismatched daemon,
/// or the errors of starting and stopping a daemon.
pub async fn ensure_daemon(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
) -> Result<Client, CliError> {
    match probe(ctx).await? {
        Probe::Running(client) => Ok(client),
        Probe::Absent => start_daemon(ctx).await,
        Probe::Mismatch => {
            let client = connect_unchecked(ctx).await?;
            let activity = status_of(&client).await.ok().map(|s| Activity::of(&s));
            if decide_restart(activity, force) == RestartDecision::AskUser
                && !confirm_restart(io, activity)?
            {
                return Err(CliError::RestartDeclined);
            }
            stop_daemon(ctx, client).await?;
            start_daemon(ctx).await
        }
    }
}

/// Starts `coda daemon` in the background and waits until it answers.
///
/// # Errors
/// Returns [`CliError::Spawn`] if the process cannot start,
/// [`CliError::DaemonExited`] if it ends before answering, or
/// [`CliError::StartTimeout`] if the daemon does not answer in time.
pub async fn start_daemon(ctx: &Context) -> Result<Client, CliError> {
    let mut child = spawn_daemon(ctx)?;
    let outcome = wait_until_running(ctx, &mut child).await;
    // Reaps the child if it ends while this process still runs.
    std::thread::spawn(move || child.wait());
    outcome
}

async fn wait_until_running(ctx: &Context, child: &mut Child) -> Result<Client, CliError> {
    let started = Instant::now();
    loop {
        if let Probe::Running(client) = probe(ctx).await? {
            return Ok(client);
        }
        if let Some(status) = child.try_wait().map_err(CliError::Output)? {
            return Err(CliError::DaemonExited {
                status,
                log: ctx.dirs.base().join(DAEMON_LOG_FILE),
            });
        }
        if started.elapsed() >= ctx.wait.timeout {
            return Err(CliError::StartTimeout {
                waited: ctx.wait.timeout,
            });
        }
        tokio::time::sleep(ctx.wait.poll).await;
    }
}

fn open_daemon_log(ctx: &Context) -> Result<File, CliError> {
    let path_error = |source| CliError::Path {
        path: ctx.dirs.base().to_path_buf(),
        source,
    };
    fs::create_dir_all(ctx.dirs.base()).map_err(path_error)?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(DAEMON_LOG_MODE)
        .open(ctx.dirs.base().join(DAEMON_LOG_FILE))
        .map_err(path_error)
}

/// The child gets its own process group so a Ctrl-C in the terminal that
/// started it does not reach the daemon.
fn spawn_daemon(ctx: &Context) -> Result<Child, CliError> {
    info!(exe = %ctx.exe.display(), "starting the daemon");
    Command::new(&ctx.exe)
        .arg(DAEMON_SUBCOMMAND)
        .env(CODA_HOME_VAR, ctx.dirs.base())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(open_daemon_log(ctx)?)
        .process_group(0)
        .spawn()
        .map_err(|source| CliError::Spawn {
            exe: ctx.exe.clone(),
            source,
        })
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use std::io::ErrorKind;

    use super::{Activity, RestartDecision, confirm_restart, decide_restart, is_absent};
    use crate::Io;

    const IDLE: Activity = Activity {
        active_turns: 0,
        other_clients: 0,
    };

    #[test]
    fn a_missing_refusing_or_resetting_socket_means_no_daemon() {
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
        ] {
            // Arrange
            let error = std::io::Error::from(kind);

            // Act
            let absent = is_absent(&error);

            // Assert
            assert!(absent, "{kind:?} should count as no daemon");
        }
        assert!(!is_absent(&std::io::Error::from(
            ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn idle_daemon_is_restarted_without_asking() {
        assert_eq!(decide_restart(Some(IDLE), false), RestartDecision::Restart);
    }

    #[test]
    fn busy_or_unknown_daemon_needs_a_yes_unless_forced() {
        // Arrange
        let turns = Activity {
            active_turns: 1,
            ..IDLE
        };
        let clients = Activity {
            other_clients: 1,
            ..IDLE
        };

        for (activity, force, expected) in [
            (Some(turns), false, RestartDecision::AskUser),
            (Some(clients), false, RestartDecision::AskUser),
            (None, false, RestartDecision::AskUser),
            (Some(turns), true, RestartDecision::Restart),
            (Some(clients), true, RestartDecision::Restart),
            (None, true, RestartDecision::Restart),
        ] {
            // Act
            let decision = decide_restart(activity, force);

            // Assert
            assert_eq!(decision, expected, "{activity:?} force={force}");
        }
    }

    #[test]
    fn only_an_explicit_yes_confirms_the_restart() {
        for (answer, expected) in [
            ("y\n", true),
            ("Yes\n", true),
            ("n\n", false),
            ("\n", false),
            ("maybe\n", false),
            ("", false),
        ] {
            // Arrange
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut input = Cursor::new(answer.as_bytes().to_vec());
            let mut io = Io {
                out: &mut out,
                err: &mut err,
                input: &mut input,
            };

            // Act
            let confirmed = confirm_restart(&mut io, Some(IDLE)).unwrap();

            // Assert
            assert_eq!(confirmed, expected, "{answer:?}");
            assert!(String::from_utf8(out).unwrap().contains("[y/N]"));
        }
    }
}
