// SPDX-License-Identifier: AGPL-3.0-only

//! The `session.set_model` request: switch an idle model-API session to a
//! model alias defined in the configuration.

use std::sync::Arc;

use coda_protocol::jsonrpc::{ErrorObject, INTERNAL_ERROR, METHOD_NOT_FOUND};
use coda_protocol::methods::{DaemonBackendsResult, SessionSetModelParams, SessionSetModelResult};
use coda_protocol::types::SessionInfo;
use coda_providers::registry::RegistryError;
use coda_store::StoreError;
use serde_json::Value;
use tracing::error;

use crate::dispatch::{HandlerResult, error, parse_params, to_value};
use crate::model_session::SwitchError;
use crate::subscribe::HubError;
use crate::turn::{BACKEND_UNSUPPORTED, SESSION_NOT_FOUND, TURN_RUNNING, TurnError, Turns};

/// The alias is not defined, or names a provider the session cannot use.
pub const UNKNOWN_MODEL: i64 = -32005;
/// The alias is valid but its provider cannot be used now, for example
/// because it is disabled or its key variable is unset.
pub const MODEL_UNAVAILABLE: i64 = -32006;

/// Answers `session.set_model`. Switching to the alias the session already
/// has is allowed and starts a new cache prefix again. A cancelled turn is
/// waited for, as `session.close` and `session.send` do.
pub(crate) async fn set_model(turns: Option<&Arc<Turns>>, params: Option<Value>) -> HandlerResult {
    let turns = turns.ok_or_else(|| error(METHOD_NOT_FOUND, "Method not found"))?;
    let params: SessionSetModelParams = parse_params(params)?;
    turns
        .await_wind_down(&params.session_id)
        .await
        .map_err(TurnError::into_rpc)?;
    let record = turns
        .models
        .set_model(&params.session_id, &params.model)
        .await
        .map_err(into_rpc)?;
    to_value(&SessionSetModelResult {
        session: SessionInfo {
            id: record.id,
            backend: record.backend,
            model: record.model,
            cwd: record.cwd,
            mode: record.mode,
        },
    })
}

/// Answers `daemon.backends` with the backends the configuration enables.
pub(crate) fn backends(turns: Option<&Arc<Turns>>) -> HandlerResult {
    let turns = turns.ok_or_else(|| error(METHOD_NOT_FOUND, "Method not found"))?;
    to_value(&DaemonBackendsResult {
        backends: turns.models.available_backends(),
    })
}

/// The wire error of a failed switch. Messages name the alias and the key
/// variable of the one provider involved, never a secret or another variable.
pub(crate) fn into_rpc(failure: SwitchError) -> ErrorObject {
    match failure {
        SwitchError::Busy(_) => error(TURN_RUNNING, "A turn is already running"),
        SwitchError::NotAModelBackend(_) => error(BACKEND_UNSUPPORTED, "Backend not supported"),
        SwitchError::Registry(e) => registry_error(&e),
        SwitchError::Hub(HubError::Store(StoreError::SessionNotFound { .. })) => {
            error(SESSION_NOT_FOUND, "Session not found")
        }
        SwitchError::Hub(e) => {
            error!(error = %e, "model switch failed");
            error(INTERNAL_ERROR, "Internal error")
        }
    }
}

/// The wire error of a registry failure, with its text, which names the alias
/// and the one key variable involved but no secret.
pub(crate) fn registry_error(failure: &RegistryError) -> ErrorObject {
    let code = match failure {
        RegistryError::UnknownAlias(_)
        | RegistryError::UnknownProvider { .. }
        | RegistryError::WrongBackend { .. } => UNKNOWN_MODEL,
        _ => MODEL_UNAVAILABLE,
    };
    error(code, &failure.to_string())
}
