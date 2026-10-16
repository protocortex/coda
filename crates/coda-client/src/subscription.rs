// SPDX-License-Identifier: AGPL-3.0-only

use coda_protocol::events::EVENT_NOTIFICATION;
use coda_protocol::jsonrpc::Notification;
use coda_protocol::types::{RequestId, SessionId};
use tokio::sync::mpsc;

use crate::connection::PERMISSION_REQUEST_NOTIFICATION;
use crate::seen_requests::SeenRequests;

/// Events and permission requests of one session, each delivered once.
#[derive(Debug)]
pub struct Subscription {
    session: SessionId,
    notifications: mpsc::Receiver<Notification>,
    last_seq: Option<u64>,
    seen_requests: SeenRequests,
}

impl Subscription {
    pub(crate) fn new(
        session: SessionId,
        notifications: mpsc::Receiver<Notification>,
        last_seq: Option<u64>,
        seen_requests: SeenRequests,
    ) -> Self {
        Self {
            session,
            notifications,
            last_seq,
            seen_requests,
        }
    }

    /// The next notification, or `None` once the connection is lost or the
    /// subscription was dropped for not being read fast enough.
    pub async fn next(&mut self) -> Option<Notification> {
        let notification = self.notifications.recv().await?;
        let params = notification.params.as_ref();
        match notification.method.as_str() {
            EVENT_NOTIFICATION => {
                if let Some(seq) = params
                    .and_then(|params| params.get("seq"))
                    .and_then(serde_json::Value::as_u64)
                {
                    self.last_seq = Some(seq);
                }
            }
            PERMISSION_REQUEST_NOTIFICATION => {
                if let Some(id) = params
                    .and_then(|params| params.get("request_id"))
                    .and_then(serde_json::Value::as_str)
                {
                    self.seen_requests.insert(RequestId::new(id));
                }
            }
            _ => {}
        }
        Some(notification)
    }

    pub(crate) fn seen_requests(&self) -> &SeenRequests {
        &self.seen_requests
    }

    /// The session this subscription follows.
    #[must_use]
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// Sequence number of the last event returned by [`Self::next`], the cursor to resume from.
    #[must_use]
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }
}
