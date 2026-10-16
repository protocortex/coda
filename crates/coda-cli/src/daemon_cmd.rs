// SPDX-License-Identifier: AGPL-3.0-only

//! `coda daemon` in the foreground and `coda daemon restart`.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use coda_config::config::{Config, ConfigError, Dirs};
use coda_daemon::lock::LockError;
use coda_daemon::server::{BoundServer, Server, ServerConfig, ServerError};
use coda_daemon::state::DaemonState;
use coda_daemon::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};
use coda_store::Store;
use coda_telemetry::init::{TelemetryConfig, init};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{info, warn};

use crate::autostart::{Activity, connect_unchecked, describe, status_of, stop_daemon};
use crate::context::{Context, Io};
use crate::error::CliError;

/// A restarting daemon may still hold the instance lock for a moment.
const BIND_ATTEMPTS: u32 = 20;
const BIND_BACKOFF: Duration = Duration::from_millis(50);

/// Runs the daemon until SIGINT, SIGTERM or a `daemon.shutdown` request.
///
/// # Errors
/// Returns the configuration, telemetry, store and server errors of starting up.
pub async fn serve(ctx: &Context) -> Result<(), CliError> {
    info!("step 1: loading the configuration");
    let config = load_config(&ctx.dirs)?;
    let _telemetry = init(&TelemetryConfig {
        otlp_endpoint: config.telemetry.otlp_endpoint.clone(),
        capture_content: config.telemetry.capture_content,
    })?;
    info!("step 2: opening the database");
    let data_dir = ctx.dirs.data_dir();
    let store = Store::open(&data_dir)?;
    let uid = own_uid(&data_dir)?;
    info!("step 3: binding the socket");
    let state = DaemonState::new(data_dir);
    let bound = bind(ServerConfig::new(ctx.dirs.socket_path(), uid))
        .await?
        .with_hub(Hub::new(store, DEFAULT_QUEUE_CAPACITY))
        .with_state(state.clone());
    let mut interrupt = signal(SignalKind::interrupt()).map_err(CliError::Signal)?;
    let mut terminate = signal(SignalKind::terminate()).map_err(CliError::Signal)?;
    info!("step 4: serving");
    bound
        .serve(async {
            tokio::select! {
                _ = interrupt.recv() => info!("interrupted"),
                _ = terminate.recv() => info!("terminated"),
                () = state.shutdown_requested() => info!("shutdown requested"),
            }
        })
        .await;
    info!("daemon stopped");
    Ok(())
}

fn load_config(dirs: &Dirs) -> Result<Config, CliError> {
    match Config::load(&dirs.config_file()) {
        Ok(parsed) => {
            for key in &parsed.unknown_keys {
                warn!(key, "ignoring an unknown config key");
            }
            Ok(parsed.config)
        }
        Err(ConfigError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(Config::default())
        }
        Err(e) => Err(e.into()),
    }
}

/// The effective uid, read from a file this process just created.
fn own_uid(dir: &std::path::Path) -> Result<u32, CliError> {
    let path_error = |source| CliError::Path {
        path: dir.to_path_buf(),
        source,
    };
    let probe = tempfile::tempfile_in(dir).map_err(path_error)?;
    Ok(probe.metadata().map_err(path_error)?.uid())
}

async fn bind(config: ServerConfig) -> Result<BoundServer, CliError> {
    let mut attempt = 1;
    loop {
        match Server::bind(config.clone()) {
            Err(ServerError::Lock(LockError::AlreadyRunning { .. })) if attempt < BIND_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(BIND_BACKOFF).await;
            }
            other => return Ok(other?),
        }
    }
}

/// Stops the running daemon. A busy daemon is only stopped with `force`.
///
/// # Errors
/// Returns [`CliError::NotRunning`] when no daemon answers, [`CliError::DaemonBusy`]
/// when turns or clients would be cut off, or the errors of stopping the daemon.
pub async fn restart(ctx: &Context, io: &mut Io<'_>, force: bool) -> Result<(), CliError> {
    let client = connect_unchecked(ctx).await?;
    // A daemon that cannot report its state may be mid-turn, so it counts as busy.
    let activity = status_of(&client).await.ok().map(|s| Activity::of(&s));
    if !activity.is_some_and(Activity::is_idle) && !force {
        return Err(CliError::DaemonBusy {
            activity: describe(activity),
        });
    }
    stop_daemon(ctx, client).await?;
    writeln!(io.out, "daemon stopped; start it with `coda daemon`")?;
    Ok(())
}
