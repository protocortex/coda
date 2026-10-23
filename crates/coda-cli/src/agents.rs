// SPDX-License-Identifier: AGPL-3.0-only

//! `coda agents`: lint definition files, list them, and record trust in a
//! repository's project definitions.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use coda_agents::{Catalog, CommandDef, CommandLookupError, revoke_project, trust_project};
use coda_protocol::paths::repo_identity;
use coda_store::{DB_FILE_NAME, Store};
use coda_tui::permission::sanitize;
use tracing::debug;

use crate::cli::{AgentsCommand, TrustArgs};
use crate::context::{Context, Io};
use crate::error::CliError;
use crate::exit;

/// Runs one `coda agents` subcommand.
///
/// # Errors
/// Returns [`CliError::NoGitRoot`] when trust is asked for outside a
/// repository, [`CliError::Store`] when the trust decision cannot be read or
/// written, or [`CliError::Output`] when output cannot be written.
pub fn run(ctx: &Context, io: &mut Io<'_>, command: &AgentsCommand) -> Result<ExitCode, CliError> {
    match command {
        AgentsCommand::Lint => lint(ctx, io),
        AgentsCommand::List => list(ctx, io).map(|()| ExitCode::SUCCESS),
        AgentsCommand::Trust(args) => trust(ctx, io, args).map(|()| ExitCode::SUCCESS),
    }
}

/// The repository root of the current directory, if it is in one.
fn repo(ctx: &Context) -> Option<PathBuf> {
    repo_identity(&ctx.cwd)
}

/// Reports every problem in the user and project files, trusted or not, since
/// linting only reads them. Project files that are held back are named too,
/// with the reason, but do not fail the lint.
fn lint(ctx: &Context, io: &mut Io<'_>) -> Result<ExitCode, CliError> {
    let repo = repo(ctx);
    let problems = Catalog::check_all(ctx.dirs.base(), repo.as_deref());
    for problem in &problems {
        writeln!(io.err, "{}", printable(&problem.to_string()))?;
    }
    if let Some(reason) = held_back(ctx, repo.as_deref())? {
        writeln!(
            io.err,
            "note: project definitions are not in use: {reason}; run `coda agents trust` after reviewing them"
        )?;
    }
    if problems.is_empty() {
        writeln!(io.out, "no problems found")?;
        return Ok(ExitCode::SUCCESS);
    }
    Ok(ExitCode::from(exit::LINT_FAILURES))
}

/// Definition files are untrusted, so their text may hold terminal escape
/// sequences; only printable characters reach the terminal.
fn printable(text: &str) -> String {
    sanitize(text).0
}

/// Why the project's definitions are pending, when they are.
fn held_back(ctx: &Context, repo: Option<&Path>) -> Result<Option<&'static str>, CliError> {
    let catalog = load_catalog(ctx, repo)?;
    Ok(catalog.pending().first().map(|p| p.reason.as_str()))
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

/// The definitions usable from `cwd`: the user's, and those of its repository
/// when the user trusted them.
pub(crate) fn catalog_at(ctx: &Context, cwd: &Path) -> Result<Catalog, CliError> {
    load_catalog(ctx, repo_identity(cwd).as_deref())
}

/// The prompt of the command file `name` with `args` in place, for a run in `cwd`.
///
/// # Errors
/// Returns [`CliError::UnknownCommand`] when no command has that name,
/// [`CliError::CommandRefused`] when only a project file waiting for trust has
/// it, or the errors of loading the definitions.
pub(crate) fn command_def(ctx: &Context, cwd: &Path, name: &str) -> Result<CommandDef, CliError> {
    let catalog = catalog_at(ctx, cwd)?;
    let entry = catalog
        .runnable_command(name)
        .map_err(|refusal| match refusal {
            CommandLookupError::Unknown { name } => CliError::UnknownCommand {
                name,
                lint_hint: catalog.problems_hint(),
            },
            pending @ CommandLookupError::Pending { .. } => CliError::CommandRefused(pending),
        })?;
    Ok(entry.def.clone())
}

/// The prompt of the command `def` named `name`, with `args` in place.
///
/// # Errors
/// Returns [`CliError::CommandArguments`] when `args` do not fit the template
/// or leave the prompt blank.
pub(crate) fn command_prompt(def: &CommandDef, name: &str, args: &str) -> Result<String, CliError> {
    def.expand(args)
        .map_err(|source| CliError::CommandArguments {
            name: name.to_owned(),
            source,
        })
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
