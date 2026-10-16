// SPDX-License-Identifier: AGPL-3.0-only

//! Connection state shared with the reader and writer tasks: in-flight requests
//! and subscriptions, including de-duplication of replayed notifications.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use coda_protocol::codec::{FrameDecoder, decode_message};
use coda_protocol::events::EVENT_NOTIFICATION;
use coda_protocol::jsonrpc::{Id, Message, Notification, Outcome};
use coda_protocol::types::{RequestId, SessionId};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::ClientError;
use crate::seen_requests::SeenRequests;

const READ_BUFFER_BYTES: usize = 8 * 1024;
/// Notification method for a new permission request.
pub(crate) const PERMISSION_REQUEST_NOTIFICATION: &str = "permission.request";

struct Subscriber {
    token: u64,
    tx: mpsc::Sender<Notification>,
    last_seq: u64,
    seen_requests: SeenRequests,
}

#[derive(Default)]
struct State {
    closed: bool,
    pending: HashMap<i64, oneshot::Sender<Outcome>>,
    subscribers: HashMap<SessionId, Subscriber>,
}

#[derive(Deserialize)]
struct EventHeader {
    session_id: SessionId,
    seq: u64,
}

#[derive(Deserialize)]
struct PermissionHeader {
    session_id: SessionId,
    request_id: RequestId,
}

#[derive(Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
    next_token: AtomicU64,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers an in-flight request; fails if the connection is already closed.
    pub(crate) fn register_pending(
        &self,
        id: i64,
    ) -> Result<oneshot::Receiver<Outcome>, ClientError> {
        let mut state = self.state();
        if state.closed {
            return Err(ClientError::Disconnected);
        }
        let (tx, rx) = oneshot::channel();
        state.pending.insert(id, tx);
        Ok(rx)
    }

    pub(crate) fn forget_pending(&self, id: i64) {
        self.state().pending.remove(&id);
    }

    /// Routes events and permission requests of `session` into the returned
    /// channel. Events at or before `since_seq` and requests in `seen_requests`
    /// are dropped as replays. The token identifies this registration for
    /// [`Self::remove_subscriber`].
    ///
    /// # Errors
    /// Returns [`ClientError::AlreadySubscribed`] if `session` has a live subscriber.
    pub(crate) fn add_subscriber(
        &self,
        session: &SessionId,
        since_seq: u64,
        seen_requests: SeenRequests,
        capacity: usize,
    ) -> Result<(u64, mpsc::Receiver<Notification>), ClientError> {
        let (tx, rx) = mpsc::channel(capacity);
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state();
        if state.closed {
            // The sender is dropped here, so the receiver ends immediately.
            return Ok((token, rx));
        }
        if state
            .subscribers
            .get(session)
            .is_some_and(|existing| !existing.tx.is_closed())
        {
            return Err(ClientError::AlreadySubscribed {
                session: session.clone(),
            });
        }
        state.subscribers.insert(
            session.clone(),
            Subscriber {
                token,
                tx,
                last_seq: since_seq,
                seen_requests,
            },
        );
        Ok((token, rx))
    }

    /// Removes the registration made with `token`, leaving any newer one alone.
    pub(crate) fn remove_subscriber(&self, session: &SessionId, token: u64) {
        let mut state = self.state();
        if state
            .subscribers
            .get(session)
            .is_some_and(|subscriber| subscriber.token == token)
        {
            state.subscribers.remove(session);
        }
    }

    /// Fails every in-flight request and ends every subscription.
    fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        state.pending.clear();
        state.subscribers.clear();
    }

    fn dispatch(&self, message: Message) {
        match message {
            Message::Response(response) => {
                let Id::Number(id) = response.id else {
                    warn!("ignoring a response with a non numeric id");
                    return;
                };
                if let Some(tx) = self.state().pending.remove(&id) {
                    let _ = tx.send(response.outcome);
                }
            }
            Message::Notification(notification) => self.route(notification),
            _ => debug!("ignoring a message the client does not handle"),
        }
    }

    fn route(&self, notification: Notification) {
        let params = notification.params.clone().unwrap_or_default();
        match notification.method.as_str() {
            EVENT_NOTIFICATION => match serde_json::from_value::<EventHeader>(params) {
                Ok(header) => self.deliver(&header.session_id, notification, |subscriber| {
                    let fresh = header.seq > subscriber.last_seq;
                    subscriber.last_seq = subscriber.last_seq.max(header.seq);
                    fresh
                }),
                Err(e) => warn!(error = %e, "dropping a malformed event notification"),
            },
            PERMISSION_REQUEST_NOTIFICATION => {
                match serde_json::from_value::<PermissionHeader>(params) {
                    Ok(header) => self.deliver(&header.session_id, notification, |subscriber| {
                        subscriber.seen_requests.insert(header.request_id.clone())
                    }),
                    Err(e) => warn!(error = %e, "dropping a malformed permission notification"),
                }
            }
            other => debug!(method = other, "ignoring an unknown notification"),
        }
    }

    /// Hands `notification` to the session's subscriber when `is_new` accepts it.
    /// A subscriber that stops reading is dropped, which ends its stream; the
    /// consumer recovers by resuming from its last seen sequence number.
    fn deliver(
        &self,
        session: &SessionId,
        notification: Notification,
        is_new: impl FnOnce(&mut Subscriber) -> bool,
    ) {
        let mut state = self.state();
        let Some(subscriber) = state.subscribers.get_mut(session) else {
            return;
        };
        if !is_new(subscriber) {
            debug!(session = %session, "dropping a replayed notification");
            return;
        }
        if subscriber.tx.try_send(notification).is_err() {
            warn!(session = %session, "ending a subscription that is not being read");
            state.subscribers.remove(session);
        }
    }
}

/// Reads frames until the socket closes, then fails everything still waiting.
pub(crate) async fn read_loop(mut reader: OwnedReadHalf, shared: Arc<Shared>) {
    let mut decoder = FrameDecoder::new();
    let mut buffer = [0u8; READ_BUFFER_BYTES];
    'read: loop {
        // Never read past the decoder's room, so a frame that completes near the
        // size limit is not rejected for the start of the next one riding along.
        let room = decoder.remaining_capacity().min(READ_BUFFER_BYTES);
        match reader.read(&mut buffer[..room]).await {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = decoder.push(&buffer[..n]) {
                    warn!(error = %e, "closing the connection");
                    break;
                }
            }
            Err(e) => {
                debug!(error = %e, "read from the daemon failed");
                break;
            }
        }
        loop {
            match decoder.next_frame() {
                Ok(Some(frame)) => match decode_message(&frame) {
                    Ok(message) => shared.dispatch(message),
                    Err(e) => warn!(error = %e, "skipping an undecodable frame"),
                },
                Ok(None) => break,
                Err(e) => {
                    warn!(error = %e, "closing the connection");
                    break 'read;
                }
            }
        }
    }
    shared.close();
}

/// Writes queued frames in order; closes the connection state on a write failure.
pub(crate) async fn write_loop(
    mut writer: OwnedWriteHalf,
    mut frames: mpsc::Receiver<Vec<u8>>,
    shared: Arc<Shared>,
) {
    while let Some(frame) = frames.recv().await {
        if let Err(e) = writer.write_all(&frame).await {
            debug!(error = %e, "write to the daemon failed");
            shared.close();
            return;
        }
    }
}
