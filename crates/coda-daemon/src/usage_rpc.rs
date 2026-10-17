// SPDX-License-Identifier: AGPL-3.0-only

//! The `usage.summary` request: sums token usage straight from the store.

use std::sync::Arc;

use coda_protocol::jsonrpc::{INTERNAL_ERROR, INVALID_PARAMS, METHOD_NOT_FOUND};
use coda_protocol::methods::{UsageSummaryParams, UsageSummaryResult};
use coda_protocol::types::{SessionId, TokenUsage};
use coda_store::{Store, StoreError};
use serde_json::Value;
use tracing::Instrument;

use crate::dispatch::{HandlerResult, error, parse_params, to_value};
use crate::subscribe::{Hub, HubError};
use crate::trace::session_span;

/// Reads the usage summary from `store`, for one session or for all of them.
///
/// The store records no cache counts or savings yet, so those report zero.
///
/// # Errors
/// Returns [`StoreError::SessionNotFound`] for an unknown session, or the
/// store's other read errors.
pub fn report(
    store: &Store,
    session: Option<&SessionId>,
) -> Result<UsageSummaryResult, StoreError> {
    let summary = store.usage_summary(session)?;
    Ok(UsageSummaryResult {
        usage: TokenUsage {
            input_tokens: summary.input_tokens,
            output_tokens: summary.output_tokens,
            ..TokenUsage::default()
        },
        saved_bytes: 0,
    })
}

/// Answers `usage.summary`. Without a hub the daemon has no store and the
/// method is unavailable. A read-only query, so a retry is safe.
pub(crate) async fn summarize(hub: Option<&Arc<Hub>>, params: Option<Value>) -> HandlerResult {
    let Some(hub) = hub else {
        return Err(error(METHOD_NOT_FOUND, "Method not found"));
    };
    let params: UsageSummaryParams = parse_params(params)?;
    let session = params.session_id;
    let read = hub.with_store({
        let session = session.clone();
        move |store| report(store, session.as_ref())
    });
    let outcome = match &session {
        Some(id) => read.instrument(session_span(id)).await,
        None => read.await,
    };
    match outcome {
        Ok(result) => to_value(&result),
        Err(HubError::Store(StoreError::SessionNotFound { .. })) => {
            Err(error(INVALID_PARAMS, "Unknown session"))
        }
        Err(e) => {
            tracing::error!(error = %e, "usage.summary failed");
            Err(error(INTERNAL_ERROR, "Internal error"))
        }
    }
}
