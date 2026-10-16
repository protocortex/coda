// SPDX-License-Identifier: AGPL-3.0-only

//! Async client for the coda daemon's JSON-RPC socket.

mod connection;
mod error;
mod seen_requests;
mod subscription;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use coda_config::timing::Timing;
use coda_protocol::codec::{CodecError, encode_message};
use coda_protocol::jsonrpc::{Id, Message, Outcome, Request, Version};
use coda_protocol::methods::{
    INITIALIZE, InitializeParams, InitializeResult, PROTOCOL_VERSION, SESSION_SUBSCRIBE,
    SessionSubscribeParams, SessionSubscribeResult,
};
use coda_protocol::types::SessionId;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tracing::debug;

use connection::{Shared, read_loop, write_loop};
pub use error::ClientError;
use seen_requests::SeenRequests;
pub use subscription::Subscription;

const CLIENT_NAME: &str = "coda-client";
const OUTBOUND_QUEUE_CAPACITY: usize = 32;
const SUBSCRIPTION_QUEUE_CAPACITY: usize = 256;

/// Removes a request from the pending map however its future ends, including cancellation.
struct PendingGuard<'a> {
    shared: &'a Shared,
    id: i64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.shared.forget_pending(self.id);
    }
}

/// A connection to the daemon. Dropping it closes the socket and stops its tasks.
pub struct Client {
    shared: Arc<Shared>,
    outbound: mpsc::Sender<Vec<u8>>,
    next_id: AtomicI64,
    request_timeout: Duration,
    _tasks: JoinSet<()>,
}

impl Client {
    /// Connects to the daemon socket and performs the `initialize` handshake.
    ///
    /// Requests on the returned client fail with [`ClientError::Timeout`] after
    /// `timing.idle_timeout`.
    ///
    /// # Errors
    /// Returns [`ClientError::Connect`] if the socket cannot be opened,
    /// [`ClientError::ProtocolMismatch`] if the daemon speaks another protocol
    /// version, or any error of [`Self::request`] from the handshake itself.
    pub async fn connect(socket_path: &Path, timing: Timing) -> Result<Self, ClientError> {
        let client = Self::connect_unchecked(socket_path, timing).await?;
        client.handshake().await?;
        Ok(client)
    }

    /// Connects without the `initialize` handshake, so a daemon that speaks
    /// another protocol version can still be inspected or stopped.
    ///
    /// # Errors
    /// Returns [`ClientError::Connect`] if the socket cannot be opened.
    pub async fn connect_unchecked(
        socket_path: &Path,
        timing: Timing,
    ) -> Result<Self, ClientError> {
        let stream =
            UnixStream::connect(socket_path)
                .await
                .map_err(|source| ClientError::Connect {
                    path: socket_path.to_path_buf(),
                    source,
                })?;
        let (reader, writer) = stream.into_split();
        let shared = Arc::new(Shared::default());
        let (outbound, frames) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let mut tasks = JoinSet::new();
        tasks.spawn(read_loop(reader, Arc::clone(&shared)));
        tasks.spawn(write_loop(writer, frames, Arc::clone(&shared)));
        Ok(Self {
            shared,
            outbound,
            next_id: AtomicI64::new(1),
            request_timeout: timing.idle_timeout,
            _tasks: tasks,
        })
    }

    async fn handshake(&self) -> Result<(), ClientError> {
        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            client_name: CLIENT_NAME.to_owned(),
        };
        let result: InitializeResult = self.request(INITIALIZE, &params).await?;
        if result.protocol_version != PROTOCOL_VERSION {
            return Err(ClientError::ProtocolMismatch {
                client: PROTOCOL_VERSION,
                daemon: result.protocol_version,
            });
        }
        debug!(daemon_version = %result.daemon_version, "connected to the daemon");
        Ok(())
    }

    /// Calls `method` and decodes its result.
    ///
    /// # Errors
    /// Returns [`ClientError::Rpc`] if the daemon answers with an error,
    /// [`ClientError::Timeout`] if no answer arrives within the request timeout,
    /// [`ClientError::Disconnected`] if the connection is lost,
    /// [`ClientError::Encode`] if `params` cannot be encoded, or
    /// [`ClientError::InvalidResult`] if the result does not decode as `R`.
    pub async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
    ) -> Result<R, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let outcome = tokio::time::timeout(self.request_timeout, self.exchange(id, method, params))
            .await
            .map_err(|_| ClientError::Timeout {
                method: method.to_owned(),
            })??;
        match outcome {
            Outcome::Result(value) => {
                serde_json::from_value(value).map_err(|source| ClientError::InvalidResult {
                    method: method.to_owned(),
                    source,
                })
            }
            Outcome::Error(error) => Err(ClientError::Rpc {
                code: error.code,
                message: error.message,
            }),
        }
    }

    async fn exchange<P: Serialize>(
        &self,
        id: i64,
        method: &str,
        params: &P,
    ) -> Result<Outcome, ClientError> {
        let encode = |source: CodecError| ClientError::Encode {
            method: method.to_owned(),
            source,
        };
        let request = Request {
            jsonrpc: Version,
            id: Id::Number(id),
            method: method.to_owned(),
            params: Some(serde_json::to_value(params).map_err(|e| encode(e.into()))?),
        };
        let frame = encode_message(&Message::Request(request)).map_err(encode)?;
        let response = self.shared.register_pending(id)?;
        let _guard = PendingGuard {
            shared: &self.shared,
            id,
        };
        self.outbound
            .send(frame)
            .await
            .map_err(|_| ClientError::Disconnected)?;
        response.await.map_err(|_| ClientError::Disconnected)
    }

    /// Follows `session`, replaying the events after `since_seq` first.
    ///
    /// # Errors
    /// Returns [`ClientError::AlreadySubscribed`] if the session already has a live
    /// subscription on this connection, or the errors of [`Self::request`], notably
    /// [`ClientError::Rpc`] for an unknown session or a cursor beyond the stored events.
    pub async fn subscribe(
        &self,
        session: &SessionId,
        since_seq: Option<u64>,
    ) -> Result<Subscription, ClientError> {
        self.subscribe_with(session, since_seq, SeenRequests::new())
            .await
    }

    async fn subscribe_with(
        &self,
        session: &SessionId,
        since_seq: Option<u64>,
        seen_requests: SeenRequests,
    ) -> Result<Subscription, ClientError> {
        // Register first so nothing pushed right after the acknowledgement is missed.
        let (token, notifications) = self.shared.add_subscriber(
            session,
            since_seq.unwrap_or_default(),
            seen_requests.clone(),
            SUBSCRIPTION_QUEUE_CAPACITY,
        )?;
        let params = SessionSubscribeParams {
            session_id: session.clone(),
            since_seq,
        };
        match self
            .request::<_, SessionSubscribeResult>(SESSION_SUBSCRIBE, &params)
            .await
        {
            Ok(_) => Ok(Subscription::new(
                session.clone(),
                notifications,
                since_seq,
                seen_requests,
            )),
            Err(e) => {
                self.shared.remove_subscriber(session, token);
                Err(e)
            }
        }
    }

    /// Re-subscribes to the session of `previous` from its last seen event, for
    /// use on a fresh connection after the old one was lost.
    ///
    /// # Errors
    /// Returns the errors of [`Self::subscribe`].
    pub async fn resume(&self, previous: &Subscription) -> Result<Subscription, ClientError> {
        self.subscribe_with(
            previous.session(),
            previous.last_seq(),
            previous.seen_requests().clone(),
        )
        .await
    }
}
