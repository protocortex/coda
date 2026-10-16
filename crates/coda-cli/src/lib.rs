// SPDX-License-Identifier: AGPL-3.0-only

//! The `coda` command line: argument parsing and the commands.

pub mod autostart;
pub mod cli;
mod context;
mod daemon_cmd;
mod error;
mod grant;
mod status;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;

use crate::cli::{Cli, Command, DaemonAction, DaemonArgs, GrantCommand};
pub use crate::context::{Context, Io, Wait};
pub use crate::error::CliError;

const USAGE_EXIT_CODE: u8 = 2;

/// Runs one `coda` invocation and returns its exit code. `args` starts with the
/// program name.
pub async fn run<I, T>(args: I, ctx: &Context, io: &mut Io<'_>) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(usage) => return report_usage(&usage, io),
    };
    match execute(cli, ctx, io).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            // Nowhere left to report a failed write to the error stream.
            let _ = writeln!(io.err, "coda: {failure}");
            ExitCode::FAILURE
        }
    }
}

fn report_usage(usage: &clap::Error, io: &mut Io<'_>) -> ExitCode {
    let text = usage.render().to_string();
    // Nowhere left to report a failed write.
    if usage.use_stderr() {
        let _ = write!(io.err, "{text}");
        ExitCode::from(USAGE_EXIT_CODE)
    } else {
        let _ = write!(io.out, "{text}");
        ExitCode::SUCCESS
    }
}

async fn execute(cli: Cli, ctx: &Context, io: &mut Io<'_>) -> Result<(), CliError> {
    let force = cli.force;
    match cli.command {
        None => status::start(ctx, io).await,
        Some(Command::Daemon(DaemonArgs { action: None })) => daemon_cmd::serve(ctx).await,
        Some(Command::Daemon(DaemonArgs {
            action: Some(DaemonAction::Restart),
        })) => daemon_cmd::restart(ctx, io, force).await,
        Some(Command::Status) => status::show(ctx, io).await,
        Some(Command::Ping) => status::ping(ctx, io).await,
        Some(Command::Run(_)) => Err(CliError::RunUnavailable),
        Some(Command::Grant(GrantCommand::Add(args))) => grant::add(ctx, io, args).await,
    }
}
