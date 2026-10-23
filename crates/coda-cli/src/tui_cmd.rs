// SPDX-License-Identifier: AGPL-3.0-only

//! The front door: `coda` and `coda tui` open the terminal UI against the daemon.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use coda_config::timing::Timing;
use coda_tui::commands::UserCommands;
use coda_tui::{Startup, TuiError};
use tracing::{info, warn};

use crate::agents::catalog_at;
use crate::autostart::ensure_daemon;
use crate::context::{Context, Io};
use crate::daemon_cmd::load_config;
use crate::error::CliError;

/// The boxed future of a TUI run.
pub type TuiFuture<'a> = Pin<Box<dyn Future<Output = Result<(), TuiError>> + 'a>>;

/// Runs the TUI on the daemon socket. `Startup` says what the config needs the
/// UI to show; the commands are the slash commands from command files.
pub type TuiLauncher = for<'a> fn(&'a Path, Timing, Startup, UserCommands) -> TuiFuture<'a>;

/// Launches the real terminal UI.
pub fn real_tui(
    socket: &Path,
    timing: Timing,
    startup: Startup,
    commands: UserCommands,
) -> TuiFuture<'_> {
    Box::pin(coda_tui::run(socket, timing, startup, commands))
}

/// Starts the daemon when absent and runs the TUI against it.
///
/// # Errors
/// Returns the errors of [`ensure_daemon`] and of reading the config, or
/// [`CliError::Tui`] when the UI fails.
pub async fn open(ctx: &Context, io: &mut Io<'_>, force: bool) -> Result<(), CliError> {
    // The TUI opens its own link; keeping this one would count as an extra client.
    drop(ensure_daemon(ctx, io, force).await?);
    let config = load_config(&ctx.dirs)?;
    let startup = Startup {
        needs_setup: !config.providers.values().any(|p| p.enabled),
        hooks_configured: !config.hooks.is_empty(),
    };
    info!(?startup, "opening the terminal UI");
    let commands = file_commands(ctx);
    let socket = ctx.dirs.socket_path();
    (ctx.launch_tui)(&socket, ctx.timing, startup, commands).await?;
    Ok(())
}

/// The slash commands from command files. A store or catalog that cannot be
/// read must not keep the user out of the UI, so it becomes a warning there.
fn file_commands(ctx: &Context) -> UserCommands {
    match catalog_at(ctx, &ctx.cwd) {
        Ok(catalog) => {
            if !catalog.problems().is_empty() {
                warn!(
                    problems = catalog.problems().len(),
                    "some definition files were skipped; run `coda agents lint` to see why"
                );
            }
            UserCommands::from_catalog(&catalog)
        }
        Err(error) => {
            warn!(%error, "the command files could not be loaded");
            UserCommands::unavailable(format!("the command files could not be loaded: {error}"))
        }
    }
}

/// `coda tui`: the UI, or a failure when there is no terminal to draw on.
///
/// # Errors
/// Returns [`CliError::NoTerminal`] without a terminal, else the errors of [`open`].
pub async fn tui(ctx: &Context, io: &mut Io<'_>, force: bool) -> Result<(), CliError> {
    if !ctx.is_tty {
        return Err(CliError::NoTerminal);
    }
    open(ctx, io, force).await
}
