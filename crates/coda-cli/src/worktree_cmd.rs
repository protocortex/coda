// SPDX-License-Identifier: Apache-2.0

//! `coda worktree clean`: report the worktrees coda made, and with `--apply`
//! remove the ones that are no longer needed.

use std::process::ExitCode;

use coda_protocol::methods::{
    CleanVerdict, WORKTREE_CLEAN, WorktreeCleanParams, WorktreeCleanResult,
};

use crate::autostart::connect_unchecked;
use crate::cli::{WorktreeCleanArgs, WorktreeCommand};
use crate::context::{Context, Io};
use crate::error::CliError;

const KIB: u64 = 1024;
const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];

/// Runs a `coda worktree` command against the running daemon; none is started.
///
/// # Errors
/// Returns the errors of reaching the daemon and of its request, including its
/// refusal to apply while `[worktree.cleanup] enabled` is off.
pub async fn run(
    ctx: &Context,
    io: &mut Io<'_>,
    command: WorktreeCommand,
) -> Result<ExitCode, CliError> {
    match command {
        WorktreeCommand::Clean(args) => clean(ctx, io, args).await?,
    }
    Ok(ExitCode::SUCCESS)
}

async fn clean(ctx: &Context, io: &mut Io<'_>, args: WorktreeCleanArgs) -> Result<(), CliError> {
    let client = connect_unchecked(ctx).await?;
    let params = WorktreeCleanParams { apply: args.apply };
    let result: WorktreeCleanResult = client.request(WORKTREE_CLEAN, &params).await?;
    if args.json {
        writeln!(io.out, "{}", serde_json::to_string(&result)?)?;
    } else {
        print(io, &result)?;
    }
    Ok(())
}

fn print(io: &mut Io<'_>, result: &WorktreeCleanResult) -> Result<(), CliError> {
    for found in &result.worktrees {
        let action = match (found.verdict, found.removed) {
            (_, true) => "removed",
            (CleanVerdict::Remove, false) if result.applied => "failed",
            (CleanVerdict::Remove, false) => "would remove",
            (CleanVerdict::Keep, _) => "keep",
            (CleanVerdict::Gone, _) => "gone",
        };
        writeln!(
            io.out,
            "{}  {action}  {}  {}  {}",
            found.session_id,
            size(found.bytes),
            found.path,
            found.reason
        )?;
        if let Some(error) = &found.error {
            writeln!(io.out, "  not removed: {error}")?;
        }
    }
    for folder in &result.unknown {
        writeln!(
            io.out,
            "unknown folder, never removed  {}  {}",
            size(folder.bytes),
            folder.path
        )?;
    }
    if !result.applied
        && result
            .worktrees
            .iter()
            .any(|w| w.verdict == CleanVerdict::Remove)
    {
        writeln!(
            io.out,
            "nothing was changed; add --apply to remove them (needs [worktree.cleanup] enabled)"
        )?;
    }
    Ok(())
}

#[allow(
    clippy::cast_precision_loss,
    reason = "a size shown to one decimal place"
)]
fn size(bytes: u64) -> String {
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= KIB as f64 && unit + 1 < UNITS.len() {
        value /= KIB as f64;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::size;

    #[test]
    fn sizes_are_shown_in_the_largest_whole_unit() {
        // Act
        let shown: Vec<_> = [0, 1023, 1024, 1536, 5 * 1024 * 1024].map(size).into();

        // Assert
        assert_eq!(shown, ["0 B", "1023 B", "1.0 KiB", "1.5 KiB", "5.0 MiB"]);
    }
}
