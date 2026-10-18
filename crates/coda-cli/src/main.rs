// SPDX-License-Identifier: AGPL-3.0-only

use std::io::{stderr, stdin, stdout};
use std::process::ExitCode;

use coda_cli::exit::FAILURE;
use coda_cli::{Context, Io, run};

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("coda: could not start the async runtime: {e}");
            return ExitCode::from(FAILURE);
        }
    };
    let ctx = match Context::from_env() {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("coda: {e}");
            return ExitCode::from(FAILURE);
        }
    };
    // Unlocked: the daemon logs to stdout from worker threads, which a lock held
    // here for the whole run would block forever.
    let (mut out, mut err) = (stdout(), stderr());
    let mut input = stdin().lock();
    let mut io = Io {
        out: &mut out,
        err: &mut err,
        input: &mut input,
    };
    runtime.block_on(run(std::env::args_os(), &ctx, &mut io))
}
