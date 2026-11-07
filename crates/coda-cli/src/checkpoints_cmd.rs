// SPDX-License-Identifier: Apache-2.0

//! `coda checkpoints prune`: report the old checkpoint refs, and with
//! `--apply` remove them.

use std::process::ExitCode;

use coda_protocol::methods::{CHECKPOINTS_PRUNE, CheckpointsPruneParams, CheckpointsPruneResult};

use crate::autostart::connect_unchecked;
use crate::cli::{CheckpointsCommand, CheckpointsPruneArgs};
use crate::context::{Context, Io};
use crate::error::CliError;
use crate::size::human;

/// Runs a `coda checkpoints` command against the running daemon; none is started.
///
/// # Errors
/// Returns the errors of reaching the daemon and of its request, including its
/// refusal to apply while `[checkpoints.prune] enabled` is off.
pub async fn run(
    ctx: &Context,
    io: &mut Io<'_>,
    command: CheckpointsCommand,
) -> Result<ExitCode, CliError> {
    match command {
        CheckpointsCommand::Prune(args) => prune(ctx, io, args).await?,
    }
    Ok(ExitCode::SUCCESS)
}

async fn prune(ctx: &Context, io: &mut Io<'_>, args: CheckpointsPruneArgs) -> Result<(), CliError> {
    let client = connect_unchecked(ctx).await?;
    let params = CheckpointsPruneParams { apply: args.apply };
    let result: CheckpointsPruneResult = client.request(CHECKPOINTS_PRUNE, &params).await?;
    if args.json {
        writeln!(io.out, "{}", serde_json::to_string(&result)?)?;
    } else {
        print(io, &result)?;
    }
    Ok(())
}

fn print(io: &mut Io<'_>, result: &CheckpointsPruneResult) -> Result<(), CliError> {
    let mut any = false;
    for session in &result.sessions {
        if let Some(note) = &session.note {
            writeln!(io.out, "session {}  left alone: {note}", session.session_id)?;
            continue;
        }
        let verb = if result.applied {
            "removed"
        } else {
            "would remove"
        };
        let count = session
            .refs
            .iter()
            .filter(|r| r.removed || !result.applied)
            .count();
        writeln!(
            io.out,
            "session {}  keeps {}  {verb} {count}  about {} for git gc to free",
            session.session_id,
            session.kept,
            human(session.bytes)
        )?;
        for pruned in &session.refs {
            any = true;
            let state = if pruned.removed || !result.applied {
                ""
            } else {
                "  (failed)"
            };
            writeln!(io.out, "  {}  {}{state}", pruned.reference, pruned.reason)?;
        }
    }
    if !result.applied && any {
        writeln!(
            io.out,
            "nothing was changed; add --apply to remove them (needs [checkpoints.prune] enabled). Run `git gc` in the repository afterwards to free the space."
        )?;
    }
    Ok(())
}
