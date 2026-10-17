// SPDX-License-Identifier: AGPL-3.0-only

//! `coda daemon` in the foreground and `coda daemon restart`.

use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use coda_config::config::{Config, ConfigError, Dirs};
use coda_daemon::backends::locate_claude_code;
use coda_daemon::lock::LockError;
use coda_daemon::reaper::reap;
use coda_daemon::server::{BoundServer, Server, ServerConfig, ServerError};
use coda_daemon::state::DaemonState;
use coda_daemon::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};
use coda_daemon::turn::{TurnConfig, Turns};
use coda_store::Store;
use coda_telemetry::init::{TelemetryConfig, init};
use tokio::signal::unix::{SignalKind, signal};
use tracing::{info, warn};

use crate::autostart::{
    Activity, connect_unchecked, describe, start_daemon, status_of, stop_daemon,
};
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
    let bound = bind(ServerConfig::new(ctx.dirs.socket_path(), uid)).await?;
    let hub = Hub::new(store, DEFAULT_QUEUE_CAPACITY);
    // The instance lock is held now, so whatever the pidfiles name is an orphan.
    info!("step 4: cleaning up after a previous daemon");
    let report = reap(&ctx.dirs.run_dir(), &hub).await;
    info!(?report, "startup cleanup finished");
    let turns = build_turns(&config, &ctx.dirs, &hub);
    let bound = bound
        .with_hub(Arc::clone(&hub))
        .with_state(state.clone())
        .with_turns(Arc::clone(&turns));
    let mut interrupt = signal(SignalKind::interrupt()).map_err(CliError::Signal)?;
    let mut terminate = signal(SignalKind::terminate()).map_err(CliError::Signal)?;
    info!("step 5: serving");
    bound
        .serve(async {
            tokio::select! {
                _ = interrupt.recv() => info!("interrupted"),
                _ = terminate.recv() => info!("terminated"),
                () = state.shutdown_requested() => info!("shutdown requested"),
            }
        })
        .await;
    turns.shutdown().await;
    info!("daemon stopped");
    Ok(())
}

/// Without the Claude Code binary the daemon still runs model-API sessions;
/// only Claude Code sessions are unavailable.
fn build_turns(config: &Config, dirs: &Dirs, hub: &Arc<Hub>) -> Arc<Turns> {
    let search_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    let binary = match locate_claude_code(config, &search_dirs) {
        Ok(binary) => {
            info!(path = %binary.path.display(), version = %binary.version, verified = binary.verified, "claude code backend ready");
            Some(binary.path)
        }
        Err(e) => {
            warn!(error = %e, "no claude code backend; its sessions can be stored but not run");
            None
        }
    };
    Turns::new(
        Arc::clone(hub),
        TurnConfig {
            binary,
            pidfile_dir: dirs.run_dir(),
            parent_env: std::env::vars_os().collect(),
            timing: (&config.timing).into(),
            models: config.clone(),
        },
    )
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

/// Stops the running daemon, if any, and starts a new one. A busy daemon is
/// only replaced with `force`.
///
/// # Errors
/// Returns [`CliError::DaemonBusy`] when turns or clients would be cut off, or the
/// errors of stopping and starting a daemon.
pub async fn restart(ctx: &Context, io: &mut Io<'_>, force: bool) -> Result<(), CliError> {
    match connect_unchecked(ctx).await {
        Ok(client) => {
            // A daemon that cannot report its state may be mid-turn, so it counts as busy.
            let activity = status_of(&client).await.ok().map(|s| Activity::of(&s));
            if !activity.is_some_and(Activity::is_idle) && !force {
                return Err(CliError::DaemonBusy {
                    activity: describe(activity),
                });
            }
            stop_daemon(ctx, client).await?;
            start_daemon(ctx).await?;
            writeln!(io.out, "daemon restarted")?;
        }
        Err(CliError::NotRunning) => {
            start_daemon(ctx).await?;
            writeln!(io.out, "daemon was not running; started it")?;
        }
        Err(e) => return Err(e),
    }
    Ok(())
}
