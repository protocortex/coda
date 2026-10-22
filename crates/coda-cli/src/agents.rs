// SPDX-License-Identifier: AGPL-3.0-only

//! `coda agents`: list definition files and record trust in a repository's
//! project definitions.

use std::fs;
use std::path::{Path, PathBuf};

use coda_agents::{Catalog, revoke_project, trust_project};
use coda_protocol::paths::repo_identity;
use coda_store::{DB_FILE_NAME, Store};
use tracing::debug;

use crate::cli::{AgentsCommand, TrustArgs};
use crate::context::{Context, Io};
use crate::error::CliError;

/// Runs one `coda agents` subcommand.
///
/// # Errors
/// Returns [`CliError::NoGitRoot`] when trust is asked for outside a
/// repository, [`CliError::Store`] when the trust decision cannot be read or
/// written, or [`CliError::Output`] when output cannot be written.
pub fn run(ctx: &Context, io: &mut Io<'_>, command: &AgentsCommand) -> Result<(), CliError> {
    match command {
        AgentsCommand::List => list(ctx, io),
        AgentsCommand::Trust(args) => trust(ctx, io, args),
    }
}

/// The repository root of the current directory, if it is in one.
fn repo(ctx: &Context) -> Option<PathBuf> {
    repo_identity(&ctx.cwd)
}

fn list(ctx: &Context, io: &mut Io<'_>) -> Result<(), CliError> {
    let catalog = load_catalog(ctx, repo(ctx).as_deref())?;
    for entry in catalog.agents() {
        writeln!(
            io.out,
            "agent {} ({})",
            entry.def.name,
            entry.source.as_str()
        )?;
    }
    for entry in catalog.skills() {
        writeln!(
            io.out,
            "skill {} ({})",
            entry.def.name,
            entry.source.as_str()
        )?;
    }
    for entry in catalog.commands() {
        writeln!(
            io.out,
            "command {} ({})",
            entry.def.name,
            entry.source.as_str()
        )?;
    }
    for pending in catalog.pending() {
        writeln!(
            io.out,
            "pending {} {}: {}; run `coda agents trust` to use it",
            pending.kind.as_str(),
            pending.path.display(),
            pending.reason.as_str()
        )?;
    }
    Ok(())
}

fn trust(ctx: &Context, io: &mut Io<'_>, args: &TrustArgs) -> Result<(), CliError> {
    let root = repo(ctx).ok_or(CliError::NoGitRoot)?;
    let key = root.display().to_string();
    let mut store = Store::open(&ctx.dirs.data_dir())?;
    if args.revoke {
        let existed = revoke_project(&mut store, &root)?;
        debug!(repo = %key, existed, "revoked project trust");
        writeln!(io.out, "no longer trusting project definitions in {key}")?;
    } else {
        trust_project(&mut store, &root)?;
        debug!(repo = %key, "trusted project definitions");
        writeln!(io.out, "trusting project definitions in {key}")?;
    }
    Ok(())
}

/// Loads the definitions with the stored trust decision for `repo`. No
/// database yet means no decision, and none is created just to read.
fn load_catalog(ctx: &Context, repo: Option<&Path>) -> Result<Catalog, CliError> {
    let data_dir = ctx.dirs.data_dir();
    if fs::metadata(data_dir.join(DB_FILE_NAME)).is_err() {
        return Ok(Catalog::load(ctx.dirs.base(), repo));
    }
    let store = Store::open(&data_dir)?;
    Ok(Catalog::load_trusted(&store, ctx.dirs.base(), repo)?)
}
