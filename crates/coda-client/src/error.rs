// SPDX-License-Identifier: AGPL-3.0-only

use std::io;
use std::path::PathBuf;

use coda_protocol::codec::CodecError;
use coda_protocol::types::SessionId;

/// Failures talking to the daemon.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// The socket could not be opened.
    #[error("could not connect to the daemon socket {}: {source}", path.display())]
    Connect {
        /// Socket that was tried.
        path: PathBuf,
        /// Underlying failure.
        source: io::Error,
    },
    /// The daemon answered `initialize` with a protocol version this client does not speak.
    #[error("the daemon speaks protocol version {daemon}, this client speaks {client}")]
    ProtocolMismatch {
        /// Version this client speaks.
        client: u32,
        /// Version the daemon reported.
        daemon: u32,
    },
    /// The session already has a live subscription on this connection.
    #[error("session {session} already has a live subscription on this connection")]
    AlreadySubscribed {
        /// The session that is already followed.
        session: SessionId,
    },
    /// The daemon answered with a JSON-RPC error.
    #[error("the daemon returned error {code}: {message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i64,
        /// The daemon's description of the error.
        message: String,
    },
    /// No response arrived within the request timeout.
    #[error("no response to `{method}` arrived in time")]
    Timeout {
        /// Method that was called.
        method: String,
    },
    /// The connection closed before or while the call was in flight.
    #[error("the connection to the daemon was lost")]
    Disconnected,
    /// The request could not be serialized or framed.
    #[error("could not encode the `{method}` request: {source}")]
    Encode {
        /// Method that was called.
        method: String,
        /// Underlying failure.
        source: CodecError,
    },
    /// The daemon's result does not have the expected shape.
    #[error("the result of `{method}` has an unexpected shape: {source}")]
    InvalidResult {
        /// Method that was called.
        method: String,
        /// Underlying failure.
        source: serde_json::Error,
    },
}
