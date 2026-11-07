// SPDX-License-Identifier: Apache-2.0

//! Idempotent commands: a request that carries a `client_request_id` is answered
//! once. A repeat gets the stored answer, so a client that lost the first reply
//! can retry without sending a second message or making a second session.

use std::sync::Arc;

use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR, INVALID_PARAMS};
use coda_protocol::methods::MAX_REQUEST_ID_BYTES;
use coda_protocol::types::SessionId;
use serde_json::Value;
use tracing::warn;

use crate::dispatch::error;
use crate::subscribe::Hub;

/// Refuses an id that is empty or longer than [`MAX_REQUEST_ID_BYTES`].
///
/// # Errors
/// Returns an invalid-params error naming the limit.
pub(crate) fn check_id(request_id: &str) -> Result<(), ErrorObject> {
    if request_id.is_empty() || request_id.len() > MAX_REQUEST_ID_BYTES {
        return Err(error(
            INVALID_PARAMS,
            &format!("client_request_id must be 1 to {MAX_REQUEST_ID_BYTES} bytes"),
        ));
    }
    Ok(())
}

/// The stored answer of `request_id`, when the same method and session already
/// answered it. An id that answered another request is refused: reusing it
/// would hand back an answer to a different question.
///
/// # Errors
/// Returns an invalid-params error for a reused id, and an internal error when
/// the store fails.
pub(crate) async fn replay(
    hub: &Arc<Hub>,
    request_id: &str,
    method: &'static str,
    session: Option<&SessionId>,
) -> Result<Option<Value>, ErrorObject> {
    let id = request_id.to_owned();
    let found = hub.with_store(move |store| store.receipt(&id)).await;
    let receipt = match found {
        Ok(found) => found,
        Err(e) => {
            warn!(error = %e, "could not read a command receipt");
            return Err(error(INTERNAL_ERROR, "Internal error"));
        }
    };
    let Some(receipt) = receipt else {
        return Ok(None);
    };
    // A create names no session before it runs, so only the others must match.
    let same_session = session.is_none() || receipt.session.as_ref() == session;
    if receipt.method != method || !same_session {
        return Err(error(
            INVALID_PARAMS,
            "client_request_id was already used for another request",
        ));
    }
    serde_json::from_str(&receipt.result_json)
        .map(Some)
        .map_err(|e| {
            warn!(error = %e, "a command receipt holds an answer that is not JSON");
            error(INTERNAL_ERROR, "Internal error")
        })
}

/// Keeps `answer` for `request_id`. A failure only costs the retry safety, so
/// it is logged, not returned.
pub(crate) async fn remember(
    hub: &Arc<Hub>,
    request_id: &str,
    method: &'static str,
    session: &SessionId,
    answer: &Value,
) {
    let (id, session, json) = (request_id.to_owned(), session.clone(), answer.to_string());
    let stored = hub
        .with_store_mut(move |store| store.record_receipt(&id, method, Some(&session), &json))
        .await;
    if let Err(e) = stored {
        warn!(error = %e, "could not keep a command receipt");
    }
}
