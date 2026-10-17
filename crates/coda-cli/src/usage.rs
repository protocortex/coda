// SPDX-License-Identifier: AGPL-3.0-only

//! `coda usage`: token usage and savings, from the daemon or else the store.

use coda_client::ClientError;
use coda_protocol::jsonrpc::METHOD_NOT_FOUND;
use coda_protocol::methods::{USAGE_SUMMARY, UsageSummaryParams, UsageSummaryResult};
use coda_protocol::types::{SessionId, TokenUsage};
use coda_store::{DB_FILE_NAME, Store};
use tracing::debug;

use crate::autostart::connect_unchecked;
use crate::cli::UsageArgs;
use crate::context::{Context, Io};
use crate::error::CliError;

/// Prints the usage summary. A daemon that answers is asked; the database is
/// opened in-process only when none is running, and no daemon is started.
///
/// # Errors
/// Returns [`CliError::DaemonTooOld`] when the running daemon predates
/// `usage.summary`, the daemon's error when it refuses the request, or
/// [`CliError::Store`] when the fallback cannot read the database.
pub async fn show(ctx: &Context, io: &mut Io<'_>, args: UsageArgs) -> Result<(), CliError> {
    let session = args.session.map(SessionId::new);
    let summary = match connect_unchecked(ctx).await {
        Ok(client) => {
            let params = UsageSummaryParams {
                session_id: session,
            };
            match client.request(USAGE_SUMMARY, &params).await {
                Err(ClientError::Rpc { code, .. }) if code == METHOD_NOT_FOUND => {
                    return Err(CliError::DaemonTooOld);
                }
                answer => answer?,
            }
        }
        Err(CliError::NotRunning) => {
            debug!("no daemon answered, reading usage from the store");
            from_store(ctx, session.as_ref())?
        }
        Err(e) => return Err(e),
    };
    print(io, &summary)
}

fn from_store(ctx: &Context, session: Option<&SessionId>) -> Result<UsageSummaryResult, CliError> {
    if !ctx.dirs.data_dir().join(DB_FILE_NAME).exists() {
        debug!("no database yet, reporting zero usage");
        return Ok(UsageSummaryResult {
            usage: TokenUsage::default(),
            saved_bytes: 0,
        });
    }
    let store = Store::open(&ctx.dirs.data_dir())?;
    Ok(coda_daemon::usage_rpc::report(&store, session)?)
}

fn print(io: &mut Io<'_>, summary: &UsageSummaryResult) -> Result<(), CliError> {
    let usage = &summary.usage;
    writeln!(io.out, "input tokens: {}", usage.input_tokens)?;
    writeln!(io.out, "output tokens: {}", usage.output_tokens)?;
    writeln!(io.out, "cache read tokens: {}", usage.cache_read_tokens)?;
    writeln!(io.out, "cache write tokens: {}", usage.cache_write_tokens)?;
    writeln!(io.out, "saved bytes: {}", summary.saved_bytes)?;
    Ok(())
}
