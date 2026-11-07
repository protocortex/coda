// SPDX-License-Identifier: Apache-2.0

//! `coda usage`: token usage and savings, from the daemon or else the store.

use std::collections::BTreeMap;

use coda_client::ClientError;
use coda_protocol::jsonrpc::METHOD_NOT_FOUND;
use coda_protocol::methods::{USAGE_SUMMARY, UsageSummaryParams, UsageSummaryResult};
use coda_protocol::types::{SessionId, TokenUsage};
use coda_providers::transcripts::{History, read_history};
use coda_store::{DB_FILE_NAME, Store};
use tracing::debug;

use crate::autostart::connect_unchecked;
use crate::cli::UsageArgs;
use crate::context::{Context, Io};
use crate::error::CliError;

const HEADER: [&str; 8] = [
    "day",
    "tool",
    "model",
    "project",
    "input",
    "output",
    "cache read",
    "cache write",
];
/// The first four columns are text; the rest are token counts.
const TEXT_COLUMNS: usize = 4;
const TOTAL_LABEL: &str = "total";

/// Prints the usage summary. A daemon that answers is asked; the database is
/// opened in-process only when none is running, and no daemon is started.
///
/// # Errors
/// Returns [`CliError::DaemonTooOld`] when the running daemon predates
/// `usage.summary`, the daemon's error when it refuses the request, or
/// [`CliError::Store`] when the fallback cannot read the database.
pub async fn show(ctx: &Context, io: &mut Io<'_>, args: UsageArgs) -> Result<(), CliError> {
    if args.history {
        return history(ctx, io).await;
    }
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

/// Prints the usage found in the vendor transcripts; no daemon or database is used.
async fn history(ctx: &Context, io: &mut Io<'_>) -> Result<(), CliError> {
    let roots = ctx.transcripts.clone();
    let found = tokio::task::spawn_blocking(move || read_history(&roots))
        .await
        .map_err(|error| CliError::Output(std::io::Error::other(error)))?;
    print_history(io, &found)
}

fn print_history(io: &mut Io<'_>, history: &History) -> Result<(), CliError> {
    if history.rows.is_empty() {
        writeln!(io.out, "no transcript usage found")?;
    } else {
        let rows: Vec<[String; 8]> = history
            .rows
            .iter()
            .map(|row| {
                cells(
                    &row.day,
                    row.source.name(),
                    &row.model,
                    &row.project,
                    &row.usage,
                )
            })
            .collect();
        write_table(io, &rows, &totals(history))?;
    }
    if history.skipped_lines > 0 {
        writeln!(
            io.out,
            "skipped {} line(s) that were not usable",
            history.skipped_lines
        )?;
    }
    if history.unreadable_files > 0 {
        writeln!(
            io.out,
            "could not read {} file(s)",
            history.unreadable_files
        )?;
    }
    Ok(())
}

/// A table row. Text from a transcript has its control characters replaced, so a
/// crafted folder name cannot write escape sequences to the terminal.
fn cells(day: &str, tool: &str, model: &str, project: &str, usage: &TokenUsage) -> [String; 8] {
    [
        printable(day),
        printable(tool),
        printable(model),
        printable(project),
        usage.input_tokens.to_string(),
        usage.output_tokens.to_string(),
        usage.cache_read_tokens.to_string(),
        usage.cache_write_tokens.to_string(),
    ]
}

fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn totals(history: &History) -> Vec<[String; 8]> {
    let mut sums: BTreeMap<&str, TokenUsage> = BTreeMap::new();
    for row in &history.rows {
        let sum = sums.entry(row.source.name()).or_default();
        sum.input_tokens += row.usage.input_tokens;
        sum.output_tokens += row.usage.output_tokens;
        sum.cache_read_tokens += row.usage.cache_read_tokens;
        sum.cache_write_tokens += row.usage.cache_write_tokens;
    }
    sums.into_iter()
        .map(|(tool, usage)| cells(TOTAL_LABEL, tool, "", "", &usage))
        .collect()
}

/// Writes the rows, a blank line and the totals under one set of column widths, the text
/// columns left aligned and the number columns right aligned.
fn write_table(
    io: &mut Io<'_>,
    rows: &[[String; 8]],
    totals: &[[String; 8]],
) -> Result<(), CliError> {
    let mut widths = HEADER.map(|name| name.chars().count());
    for row in rows.iter().chain(totals) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let line = |cells: [&str; 8]| {
        let padded: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(at, cell)| {
                let width = widths[at];
                if at < TEXT_COLUMNS {
                    format!("{cell:<width$}")
                } else {
                    format!("{cell:>width$}")
                }
            })
            .collect();
        padded.join("  ").trim_end().to_owned()
    };
    writeln!(io.out, "{}", line(HEADER))?;
    for row in rows {
        writeln!(io.out, "{}", line(row.each_ref().map(String::as_str)))?;
    }
    writeln!(io.out)?;
    for row in totals {
        writeln!(io.out, "{}", line(row.each_ref().map(String::as_str)))?;
    }
    Ok(())
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
