// SPDX-License-Identifier: AGPL-3.0-only

//! Command line the self-hosting verifier script calls.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use coda_selfhost::error::SelfhostError;
use coda_selfhost::verify::{VerifyPlan, verify_workdir};

const CARGO_LIMIT: Duration = Duration::from_mins(30);
/// Exit codes: 0 accepted, 1 rejected, 2 usage (clap), 3 verifier failure.
const REJECTED: u8 = 1;
const INFRASTRUCTURE: u8 = 3;

#[derive(Debug, Parser)]
#[command(name = "coda-selfhost")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Accept or reject a finished attempt (exit 0 or 1).
    Verify {
        /// The attempt's copy of the repository.
        #[arg(long)]
        workdir: PathBuf,
        /// Build output directory.
        #[arg(long)]
        target_dir: Option<PathBuf>,
        /// Arguments after `cargo test`; all tests when absent.
        #[arg(last = true)]
        cargo_test_args: Vec<String>,
    },
}

fn main() -> ExitCode {
    match execute(Cli::parse().command) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(REJECTED),
        Err(e) => {
            eprintln!("coda-selfhost: {e}");
            ExitCode::from(INFRASTRUCTURE)
        }
    }
}

/// `true` when the command's check passed.
fn execute(command: Command) -> Result<bool, SelfhostError> {
    match command {
        Command::Verify {
            workdir,
            target_dir,
            cargo_test_args,
        } => {
            eprintln!("[selfhost] verifying {}", workdir.display());
            let verdict = verify_workdir(&VerifyPlan {
                workdir: &workdir,
                cargo_test_args,
                target_dir: target_dir.as_deref(),
                limit: CARGO_LIMIT,
            });
            match verdict {
                Ok(()) => Ok(true),
                Err(SelfhostError::Rejected(why)) => {
                    eprintln!("[selfhost] rejected: {why}");
                    Ok(false)
                }
                Err(other) => Err(other),
            }
        }
    }
}
