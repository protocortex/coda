// SPDX-License-Identifier: AGPL-3.0-only

//! `coda grant add`: asks the daemon to store a standing permission grant.

use std::fs;

use coda_protocol::methods::{GRANT_ADD, GrantAddParams, GrantAddResult};
use coda_protocol::paths::repo_identity;

use crate::autostart::ensure_daemon;
use crate::cli::GrantAddArgs;
use crate::context::{Context, Io};
use crate::error::CliError;

/// Resolves the repository and cwd, then sends `grant.add`. A directory outside
/// any git repository is refused before the daemon is contacted.
///
/// # Errors
/// Returns [`CliError::NoGitRoot`] outside a repository, [`CliError::Path`] if a
/// directory cannot be resolved, or the errors of reaching the daemon.
pub async fn add(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    args: GrantAddArgs,
) -> Result<(), CliError> {
    let repo = ctx.cwd.join(&args.repo);
    let repo_path = repo_identity(&repo).ok_or(CliError::NoGitRoot)?;
    let cwd = args
        .cwd
        .as_deref()
        .map_or_else(|| ctx.cwd.clone(), |cwd| ctx.cwd.join(cwd));
    let cwd = fs::canonicalize(&cwd).map_err(|source| CliError::Path { path: cwd, source })?;
    let params = GrantAddParams {
        repo_path: repo_path.display().to_string(),
        tool: args.tool,
        cwd: cwd.display().to_string(),
        pattern: args.command,
    };
    let client = ensure_daemon(ctx, io, force).await?;
    let _: GrantAddResult = client.request(GRANT_ADD, &params).await?;
    writeln!(
        io.out,
        "granted `{}` for {} in {}",
        params.pattern, params.tool, params.repo_path
    )?;
    Ok(())
}
