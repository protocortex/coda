// SPDX-License-Identifier: AGPL-3.0-only

//! Per-session event fan-out: stored events are replayed from a cursor and then
//! followed live, with no gap and no duplicate between the two.
//!
//! A subscriber registers first and buffers live events while the stored log is
//! replayed; the buffer is then flushed past the last replayed sequence number
//! and the subscriber goes live under the same lock that found the buffer empty.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use coda_protocol::codec::{CodecError, encode_message};
use coda_protocol::events::EVENT_NOTIFICATION;
use coda_protocol::jsonrpc::{Message, Notification, Version};
use coda_protocol::methods::PermissionRequestParams;
use coda_protocol::types::{PermissionDecision, RequestId, SessionId, TurnId};
use coda_store::{
    EventRecord, Grant, GrantKey, NewDecision, NewPending, PendingPermission, Store, StoreError,
};
use serde::Serialize;
use tokio::sync::{Notify, mpsc};
use tracing::{debug, error, warn};

/// Notifications buffered per connection before it is disconnected as too slow.
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;
/// Stored event kind of a newly queued permission request.
pub const PERMISSION_REQUEST_KIND: &str = "permission_request";
/// Notification re-sent on resume for a request nobody has answered yet.
pub const PERMISSION_REQUEST_NOTIFICATION: &str = "permission.request";
/// Stored events read per store call while replaying.
pub const DEFAULT_REPLAY_PAGE: u32 = 500;

/// Failures publishing or subscribing.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HubError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the cursor is beyond the range of stored sequence numbers")]
    InvalidCursor,
    #[error("event subscriptions are not available on this server")]
    Unavailable,
    #[error("the connection was closed before the subscription completed")]
    Disconnected,
    #[error("could not encode a notification: {0}")]
    Encode(#[from] CodecError),
    #[error("a store task did not finish: {0}")]
    Task(#[from] tokio::task::JoinError),
}

/// The frames one stored event produces, in delivery order: the event
/// notification, then any announcement that goes with it.
struct Queued {
    seq: u64,
    frames: Vec<Vec<u8>>,
}

enum State {
    /// Replaying the log; live events wait here.
    Replaying(Vec<Queued>),
    Live,
}

#[derive(PartialEq, Eq)]
enum Delivery {
    Accepted,
    Full,
    Gone,
}

struct Entry {
    connection: u64,
    state: State,
    tx: mpsc::Sender<Vec<u8>>,
    kicked: Arc<Notify>,
}

#[derive(Serialize)]
struct EventParams<'a> {
    session_id: &'a SessionId,
    seq: u64,
    kind: &'a str,
    body: &'a str,
}

fn frame(method: &str, params: &impl Serialize) -> Result<Vec<u8>, HubError> {
    let params = serde_json::to_value(params).map_err(CodecError::from)?;
    let message = Message::Notification(Notification {
        jsonrpc: Version,
        method: method.to_owned(),
        params: Some(params),
    });
    Ok(encode_message(&message)?)
}

fn event_frame(session: &SessionId, seq: u64, record: &EventRecord) -> Result<Vec<u8>, HubError> {
    frame(
        EVENT_NOTIFICATION,
        &EventParams {
            session_id: session,
            seq,
            kind: &record.kind,
            body: &record.body,
        },
    )
}

fn pending_frame(pending: &PendingPermission) -> Result<Vec<u8>, HubError> {
    permission_frame(&PermissionRequestParams {
        session_id: pending.session.clone(),
        request_id: pending.id.clone(),
        tool: pending.tool.clone(),
        repo_path: pending.repo_path.clone(),
        command: pending.command.clone(),
    })
}

fn permission_frame(params: &PermissionRequestParams) -> Result<Vec<u8>, HubError> {
    frame(PERMISSION_REQUEST_NOTIFICATION, params)
}

/// Owns the store handle and the live subscribers; the turn loop publishes through it.
pub struct Hub {
    store: Arc<Mutex<Store>>,
    sessions: Mutex<HashMap<SessionId, Vec<Entry>>>,
    queue_capacity: usize,
    replay_page: u32,
    next_id: AtomicU64,
    #[cfg(test)]
    hook: Mutex<Option<Hook>>,
}

/// One client connection's side of the hub. Dropping it ends its subscriptions.
pub struct Connection {
    id: u64,
    hub: Option<Arc<Hub>>,
    tx: mpsc::Sender<Vec<u8>>,
    kicked: Arc<Notify>,
}

/// What the connection writer drains: queued notifications and the disconnect signal.
pub struct Outbox {
    pub events: mpsc::Receiver<Vec<u8>>,
    /// Notified when the connection fell too far behind and must be closed.
    pub kicked: Arc<Notify>,
}

/// Points in `subscribe` where tests can inject a racing publish.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Registered,
    Snapshotted,
}

#[cfg(test)]
type Hook = Arc<
    dyn Fn(Stage) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

impl Hub {
    /// Wraps `store`; each connection may queue `queue_capacity` notifications.
    #[must_use]
    pub fn new(store: Store, queue_capacity: usize) -> Arc<Self> {
        Self::with_replay_page(store, queue_capacity, DEFAULT_REPLAY_PAGE)
    }

    /// Like [`Hub::new`], reading `replay_page` stored events per store call.
    #[must_use]
    pub fn with_replay_page(store: Store, queue_capacity: usize, replay_page: u32) -> Arc<Self> {
        Arc::new(Self {
            store: Arc::new(Mutex::new(store)),
            sessions: Mutex::new(HashMap::new()),
            queue_capacity: queue_capacity.max(1),
            replay_page: replay_page.max(1),
            next_id: AtomicU64::new(1),
            #[cfg(test)]
            hook: Mutex::new(None),
        })
    }

    /// Opens a connection that can subscribe to sessions of this hub.
    #[must_use]
    pub fn connect(self: &Arc<Self>) -> (Connection, Outbox) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Connection::open(id, Some(Arc::clone(self)), self.queue_capacity)
    }

    /// Appends `record` to the session log and delivers it to subscribers.
    ///
    /// # Errors
    /// Returns [`HubError::Store`] when the append is refused.
    pub async fn publish(
        self: &Arc<Self>,
        session: &SessionId,
        turn: &TurnId,
        record: EventRecord,
    ) -> Result<u64, HubError> {
        let (hub, session, turn) = (Arc::clone(self), session.clone(), turn.clone());
        tokio::task::spawn_blocking(move || hub.append_and_fan_out(&session, &turn, &record))
            .await?
    }

    /// Persists a permission request and announces it to subscribers, as an
    /// event and as a `permission.request` notification.
    ///
    /// # Errors
    /// Returns [`HubError::Store`] when the session or turn is unknown; nothing
    /// is stored then.
    pub async fn register_pending(
        self: &Arc<Self>,
        turn: &TurnId,
        pending: NewPending,
    ) -> Result<RequestId, HubError> {
        let (hub, turn) = (Arc::clone(self), turn.clone());
        tokio::task::spawn_blocking(move || {
            // One guard for the whole sequence: subscribers never see the
            // event without its pending row, or the reverse.
            let mut store = hub.store();
            store.ensure_turn(&pending.session, &turn)?;
            let request = store.add_pending(&pending)?;
            let record = EventRecord {
                kind: PERMISSION_REQUEST_KIND.to_owned(),
                body: serde_json::json!({ "request_id": request }).to_string(),
            };
            let announcement = permission_frame(&PermissionRequestParams {
                session_id: pending.session.clone(),
                request_id: request.clone(),
                tool: pending.tool.clone(),
                repo_path: pending.repo_path.clone(),
                command: pending.command.clone(),
            })
            .inspect_err(
                |e| warn!(request = %request, error = %e, "request stored but not announced"),
            )
            .ok();
            hub.append_locked(&mut store, &pending.session, &turn, &record, announcement)?;
            Ok(request)
        })
        .await?
    }

    /// Records the answer to a pending permission request.
    ///
    /// # Errors
    /// Returns [`HubError::Store`] for an unknown or already answered request.
    pub async fn resolve_pending(
        self: &Arc<Self>,
        request: &RequestId,
        decision: PermissionDecision,
    ) -> Result<(), HubError> {
        let (hub, request) = (Arc::clone(self), request.clone());
        tokio::task::spawn_blocking(move || Ok(hub.store().resolve_pending(&request, decision)?))
            .await?
    }

    /// Stores a grant, keeping the first one if it exists already, and reads it back.
    ///
    /// # Errors
    /// Returns [`HubError::Store`] when the store refuses the write or the read.
    pub async fn add_grant(
        self: &Arc<Self>,
        repo_path: String,
        tool: String,
        cwd: String,
        pattern: String,
    ) -> Result<Option<Grant>, HubError> {
        let hub = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let key = GrantKey {
                repo_path: &repo_path,
                tool: &tool,
                cwd: &cwd,
                pattern: &pattern,
            };
            let mut store = hub.store();
            store.save_grant(&key)?;
            Ok(store.lookup_grant(&key)?)
        })
        .await?
    }

    /// Records a permission answer that did not go through a pending request.
    ///
    /// # Errors
    /// Returns [`HubError::Store`] when the session is unknown.
    pub async fn record_decision(self: &Arc<Self>, new: NewDecision) -> Result<(), HubError> {
        let hub = Arc::clone(self);
        tokio::task::spawn_blocking(move || Ok(hub.store().record_decision(&new)?)).await?
    }

    /// Runs `work` against the store on the blocking pool.
    pub(crate) async fn with_store<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, HubError> {
        let hub = Arc::clone(self);
        tokio::task::spawn_blocking(move || Ok(work(&hub.store())?)).await?
    }

    /// Runs `work` against the writable store on the blocking pool.
    pub(crate) async fn with_store_mut<T: Send + 'static>(
        self: &Arc<Self>,
        work: impl FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, HubError> {
        let hub = Arc::clone(self);
        tokio::task::spawn_blocking(move || Ok(work(&mut hub.store())?)).await?
    }

    fn store(&self) -> MutexGuard<'_, Store> {
        // SQLite rolls an interrupted transaction back, so a poisoned guard is safe to reuse.
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<SessionId, Vec<Entry>>> {
        // Plain data, so a panic elsewhere cannot leave it invalid.
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Appends and delivers under the store lock so subscribers see sequence order.
    fn append_and_fan_out(
        &self,
        session: &SessionId,
        turn: &TurnId,
        record: &EventRecord,
    ) -> Result<u64, HubError> {
        let mut store = self.store();
        self.append_locked(&mut store, session, turn, record, None)
    }

    /// Appends through a store guard the caller already holds, and delivers the
    /// event frame followed by `announcement`, if any.
    fn append_locked(
        &self,
        store: &mut Store,
        session: &SessionId,
        turn: &TurnId,
        record: &EventRecord,
        announcement: Option<Vec<u8>>,
    ) -> Result<u64, HubError> {
        let seq = store.append_event(session, turn, record)?;
        match event_frame(session, seq, record) {
            Ok(event) => {
                let frames = std::iter::once(event).chain(announcement).collect();
                self.fan_out(session, &Queued { seq, frames });
            }
            Err(e) => warn!(session = %session, seq, error = %e, "event stored but not delivered"),
        }
        Ok(seq)
    }

    /// Hands `queued` to every subscriber of `session` without ever waiting;
    /// one whose queue is full is dropped and told to disconnect.
    fn fan_out(&self, session: &SessionId, queued: &Queued) {
        let capacity = self.queue_capacity;
        let mut sessions = self.sessions();
        let Some(entries) = sessions.get_mut(session) else {
            return;
        };
        entries.retain_mut(|entry| {
            let delivery = match &mut entry.state {
                State::Replaying(backlog) if backlog.len() < capacity => {
                    backlog.push(Queued {
                        seq: queued.seq,
                        frames: queued.frames.clone(),
                    });
                    Delivery::Accepted
                }
                State::Replaying(_) => Delivery::Full,
                State::Live => queued
                    .frames
                    .iter()
                    .map(|frame| match entry.tx.try_send(frame.clone()) {
                        Ok(()) => Delivery::Accepted,
                        Err(mpsc::error::TrySendError::Full(_)) => Delivery::Full,
                        Err(mpsc::error::TrySendError::Closed(_)) => Delivery::Gone,
                    })
                    .find(|delivery| *delivery != Delivery::Accepted)
                    .unwrap_or(Delivery::Accepted),
            };
            if delivery == Delivery::Full {
                warn!(session = %session, "disconnecting a subscriber that is not reading");
                entry.kicked.notify_one();
            }
            delivery == Delivery::Accepted
        });
    }

    fn register(&self, session: &SessionId, connection: &Connection) {
        let entry = Entry {
            connection: connection.id,
            state: State::Replaying(Vec::new()),
            tx: connection.tx.clone(),
            kicked: Arc::clone(&connection.kicked),
        };
        let mut sessions = self.sessions();
        let entries = sessions.entry(session.clone()).or_default();
        entries.retain(|e| e.connection != connection.id);
        entries.push(entry);
    }

    fn unregister(&self, session: &SessionId, connection: u64) {
        let mut sessions = self.sessions();
        if let Some(entries) = sessions.get_mut(session) {
            entries.retain(|e| e.connection != connection);
            if entries.is_empty() {
                sessions.remove(session);
            }
        }
    }

    fn unregister_all(&self, connection: u64) {
        let mut sessions = self.sessions();
        sessions.retain(|_, entries| {
            entries.retain(|e| e.connection != connection);
            !entries.is_empty()
        });
    }

    /// Takes the events buffered while replaying. Returns `None` when the
    /// subscriber was dropped meanwhile, and switches it live when nothing is buffered.
    fn take_backlog(&self, session: &SessionId, connection: u64) -> Option<Vec<Queued>> {
        let mut sessions = self.sessions();
        let entry = sessions
            .get_mut(session)?
            .iter_mut()
            .find(|e| e.connection == connection)?;
        let State::Replaying(backlog) = &mut entry.state else {
            return Some(Vec::new());
        };
        if backlog.is_empty() {
            entry.state = State::Live;
            return Some(Vec::new());
        }
        Some(std::mem::take(backlog))
    }

    #[cfg(test)]
    fn set_hook(
        &self,
        hook: impl Fn(Stage) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync
        + 'static,
    ) {
        *self.hook.lock().unwrap() = Some(Arc::new(hook));
    }

    #[cfg(test)]
    async fn reach(&self, stage: Stage) {
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(stage).await;
        }
    }
}

impl Connection {
    fn open(id: u64, hub: Option<Arc<Hub>>, capacity: usize) -> (Self, Outbox) {
        let (tx, events) = mpsc::channel(capacity);
        let kicked = Arc::new(Notify::new());
        let outbox = Outbox {
            events,
            kicked: Arc::clone(&kicked),
        };
        (
            Self {
                id,
                hub,
                tx,
                kicked,
            },
            outbox,
        )
    }

    /// A connection with no hub behind it; subscribing fails.
    #[must_use]
    pub fn detached() -> (Self, Outbox) {
        Self::open(0, None, 1)
    }

    /// Queues `ack`, replays events after `since`, then follows live ones,
    /// re-sending open permission requests after the replay. `None` skips the
    /// history and delivers only events published from now on.
    ///
    /// `ack`, the caller's success response, is queued with the events so it
    /// reaches the client before any of them. Once it is queued the
    /// subscription is committed: a later failure closes the connection
    /// instead of being returned.
    ///
    /// # Errors
    /// Returns [`HubError::InvalidCursor`], [`HubError::Unavailable`], or
    /// [`HubError::Store`] for an unknown session, all before anything is queued.
    pub async fn subscribe(
        &self,
        session: &SessionId,
        since: Option<u64>,
        ack: Option<Vec<u8>>,
    ) -> Result<(), HubError> {
        let hub = self.hub.as_ref().ok_or(HubError::Unavailable)?;
        if since.is_some_and(|since| i64::try_from(since).is_err()) {
            return Err(HubError::InvalidCursor);
        }
        let target = session.clone();
        hub.with_store(move |store| store.load_session(&target).map(drop))
            .await?;
        hub.register(session, self);
        debug!(session = %session, ?since, "subscriber registered");
        if let Err(e) = self.stream(hub, session, since, ack).await {
            hub.unregister(session, self.id);
            self.kicked.notify_one();
            match e {
                HubError::Disconnected => debug!(session = %session, "subscriber went away"),
                e => {
                    error!(session = %session, error = %e, "subscription failed after it was acknowledged");
                }
            }
        }
        Ok(())
    }

    async fn stream(
        &self,
        hub: &Arc<Hub>,
        session: &SessionId,
        since: Option<u64>,
        ack: Option<Vec<u8>>,
    ) -> Result<(), HubError> {
        if let Some(ack) = ack {
            self.send(ack).await?;
        }
        #[cfg(test)]
        hub.reach(Stage::Registered).await;
        let mut last = match since {
            Some(since) => self.send_stored(hub, session, since).await?,
            None => 0,
        };
        #[cfg(test)]
        hub.reach(Stage::Snapshotted).await;
        self.send_open_requests(hub, session).await?;
        // Live events that raced the replay wait in the backlog; the loop ends
        // only once a look at it found it empty, which also switches to live.
        while let Some(backlog) = hub.take_backlog(session, self.id) {
            if backlog.is_empty() {
                return Ok(());
            }
            let floor = last;
            for queued in backlog.into_iter().filter(|q| q.seq > floor) {
                last = queued.seq;
                for frame in queued.frames {
                    self.send(frame).await?;
                }
            }
        }
        Err(HubError::Disconnected)
    }

    /// Sends stored events after `since`; returns the last sequence number sent.
    async fn send_stored(
        &self,
        hub: &Arc<Hub>,
        session: &SessionId,
        since: u64,
    ) -> Result<u64, HubError> {
        let page_size = hub.replay_page;
        let mut last = since;
        loop {
            let target = session.clone();
            let after = last;
            let page = hub
                .with_store(move |store| store.load_events(&target, after, page_size))
                .await?;
            for stored in &page {
                let record = EventRecord {
                    kind: stored.kind.clone(),
                    body: stored.body.clone(),
                };
                self.send(event_frame(session, stored.seq, &record)?)
                    .await?;
                last = stored.seq;
            }
            if page.len() < page_size as usize {
                return Ok(last);
            }
        }
    }

    async fn send_open_requests(
        &self,
        hub: &Arc<Hub>,
        session: &SessionId,
    ) -> Result<(), HubError> {
        let target = session.clone();
        let open = hub
            .with_store(move |store| store.list_pending(&target))
            .await?;
        for pending in &open {
            self.send(pending_frame(pending)?).await?;
        }
        Ok(())
    }

    /// Waits for room: replay may legitimately exceed the live queue, and a
    /// stalled writer ends the connection through its write timeout. Waiting
    /// for spare room beyond this frame keeps a client that reads steadily from
    /// reaching the live phase with a full queue and being dropped as slow.
    async fn send(&self, frame: Vec<u8>) -> Result<(), HubError> {
        let capacity = self.tx.max_capacity();
        let mut permits = self
            .tx
            .reserve_many((capacity / 2 + 1).min(capacity))
            .await
            .map_err(|_| HubError::Disconnected)?;
        if let Some(permit) = permits.next() {
            permit.send(frame);
        }
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(hub) = &self.hub {
            hub.unregister_all(self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_protocol::codec::decode_message;
    use coda_protocol::jsonrpc::Message;
    use coda_protocol::types::{BackendKind, Mode};
    use coda_store::NewSession;
    use serde_json::Value;
    use std::time::Duration;
    use tempfile::TempDir;

    const RECV_TIMEOUT: Duration = Duration::from_secs(5);
    const QUIET: Duration = Duration::from_millis(100);
    /// Length of the frame header in front of each queued payload.
    const HEADER_LEN: usize = 4;

    struct Fixture {
        _dir: TempDir,
        hub: Arc<Hub>,
        session: SessionId,
        turn: TurnId,
    }

    fn new_session(store: &mut Store, cwd: &str) -> (SessionId, TurnId) {
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::ClaudeCode,
                model: None,
                cwd: cwd.to_owned(),
                mode: Mode::default(),
            })
            .unwrap();
        let turn = store.begin_turn(&session).unwrap();
        (session, turn)
    }

    fn fixture(capacity: usize) -> Fixture {
        fixture_paged(capacity, DEFAULT_REPLAY_PAGE)
    }

    fn fixture_paged(capacity: usize, page: u32) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::open(&dir.path().join("data")).unwrap();
        let (session, turn) = new_session(&mut store, "/work");
        Fixture {
            _dir: dir,
            hub: Hub::with_replay_page(store, capacity, page),
            session,
            turn,
        }
    }

    fn record(body: &str) -> EventRecord {
        EventRecord {
            kind: "stream_delta".to_owned(),
            body: body.to_owned(),
        }
    }

    impl Fixture {
        async fn publish(&self, n: u32) -> u64 {
            self.hub
                .publish(&self.session, &self.turn, record(&format!("e{n}")))
                .await
                .unwrap()
        }

        async fn publish_many(&self, count: u32) {
            for n in 1..=count {
                self.publish(n).await;
            }
        }

        fn pending(&self, command: &str) -> NewPending {
            NewPending {
                session: self.session.clone(),
                tool: "bash".to_owned(),
                repo_path: "/work".to_owned(),
                command: command.to_owned(),
            }
        }

        /// Makes `subscribe` publish one event when it reaches `at`.
        fn race_publish_at(&self, at: Stage) {
            self.race_publish_many_at(at, 1);
        }

        /// Makes `subscribe` publish `count` events when it reaches `at`.
        fn race_publish_many_at(&self, at: Stage, count: u32) {
            let (hub, session, turn) = (
                Arc::clone(&self.hub),
                self.session.clone(),
                self.turn.clone(),
            );
            self.hub.set_hook(move |stage| {
                let (hub, session, turn) = (Arc::clone(&hub), session.clone(), turn.clone());
                Box::pin(async move {
                    if stage == at {
                        for _ in 0..count {
                            hub.publish(&session, &turn, record("raced")).await.unwrap();
                        }
                    }
                })
            });
        }
    }

    async fn next_params(outbox: &mut Outbox) -> (String, Value) {
        let frame = tokio::time::timeout(RECV_TIMEOUT, outbox.events.recv())
            .await
            .expect("timed out waiting for a notification")
            .expect("outbox closed");
        match decode_message(&frame[HEADER_LEN..]).unwrap() {
            Message::Notification(n) => (n.method, n.params.unwrap()),
            other => panic!("expected a notification, got {other:?}"),
        }
    }

    async fn next_seq(outbox: &mut Outbox) -> u64 {
        let (method, params) = next_params(outbox).await;
        assert_eq!(method, "session.event");
        params["seq"].as_u64().unwrap()
    }

    async fn assert_quiet(outbox: &mut Outbox) {
        let got = tokio::time::timeout(QUIET, outbox.events.recv()).await;
        assert!(got.is_err(), "expected no further notifications");
    }

    #[tokio::test]
    async fn resuming_from_cursor_3_of_5_yields_4_and_5_once() {
        // Arrange
        let f = fixture(8);
        f.publish_many(5).await;
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, Some(3), None).await.unwrap();

        // Assert
        assert_eq!(next_seq(&mut outbox).await, 4);
        assert_eq!(next_seq(&mut outbox).await, 5);
        assert_quiet(&mut outbox).await;
    }

    #[tokio::test]
    async fn cursor_0_replays_everything() {
        // Arrange
        let f = fixture(8);
        f.publish_many(3).await;
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, Some(0), None).await.unwrap();

        // Assert
        for expected in 1..=3 {
            assert_eq!(next_seq(&mut outbox).await, expected);
        }
        assert_quiet(&mut outbox).await;
    }

    #[tokio::test]
    async fn cursor_past_the_end_replays_nothing_then_follows_live() {
        // Arrange
        let f = fixture(8);
        f.publish_many(2).await;
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, Some(99), None).await.unwrap();
        assert_quiet(&mut outbox).await;
        f.publish(3).await;

        // Assert
        assert_eq!(next_seq(&mut outbox).await, 3);
    }

    #[tokio::test]
    async fn empty_log_then_live_event() {
        // Arrange
        let f = fixture(8);
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, Some(0), None).await.unwrap();
        assert_quiet(&mut outbox).await;
        f.publish(1).await;

        // Assert
        assert_eq!(next_seq(&mut outbox).await, 1);
    }

    #[tokio::test]
    async fn cursor_beyond_the_store_range_is_invalid() {
        // Arrange
        let f = fixture(8);
        let (conn, _outbox) = f.hub.connect();

        // Act
        let result = conn.subscribe(&f.session, Some(u64::MAX), None).await;

        // Assert
        assert!(matches!(result, Err(HubError::InvalidCursor)));
    }

    #[tokio::test]
    async fn unknown_session_is_refused() {
        // Arrange
        let f = fixture(8);
        let (conn, _outbox) = f.hub.connect();

        // Act
        let result = conn.subscribe(&SessionId::new("404"), Some(0), None).await;

        // Assert
        assert!(matches!(
            result,
            Err(HubError::Store(StoreError::SessionNotFound { .. }))
        ));
    }

    #[tokio::test]
    async fn detached_connection_cannot_subscribe() {
        // Arrange
        let (conn, _outbox) = Connection::detached();

        // Act
        let result = conn.subscribe(&SessionId::new("1"), Some(0), None).await;

        // Assert
        assert!(matches!(result, Err(HubError::Unavailable)));
    }

    #[tokio::test]
    async fn event_published_between_snapshot_and_going_live_appears_once() {
        // Arrange
        let f = fixture(8);
        f.publish_many(2).await;
        f.race_publish_at(Stage::Snapshotted);
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, Some(0), None).await.unwrap();

        // Assert
        for expected in 1..=3 {
            assert_eq!(next_seq(&mut outbox).await, expected);
        }
        assert_quiet(&mut outbox).await;
    }

    #[tokio::test]
    async fn event_published_right_after_registering_appears_once() {
        // Arrange
        let f = fixture(8);
        f.publish_many(2).await;
        f.race_publish_at(Stage::Registered);
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, Some(0), None).await.unwrap();

        // Assert
        for expected in 1..=3 {
            assert_eq!(next_seq(&mut outbox).await, expected);
        }
        assert_quiet(&mut outbox).await;
    }

    #[tokio::test]
    async fn client_that_never_reads_is_disconnected_while_others_keep_receiving() {
        // Arrange
        let f = fixture(2);
        let (slow, slow_outbox) = f.hub.connect();
        let (fast, mut fast_outbox) = f.hub.connect();
        slow.subscribe(&f.session, Some(0), None).await.unwrap();
        fast.subscribe(&f.session, Some(0), None).await.unwrap();

        // Act
        for n in 1..=4 {
            f.publish(n).await;
            assert_eq!(next_seq(&mut fast_outbox).await, u64::from(n));
        }

        // Assert
        tokio::time::timeout(RECV_TIMEOUT, slow_outbox.kicked.notified())
            .await
            .expect("the slow client was not disconnected");
        f.publish(5).await;
        assert_eq!(next_seq(&mut fast_outbox).await, 5);
    }

    #[tokio::test]
    async fn pending_request_is_persisted_and_resent_on_resume() {
        // Arrange
        let f = fixture(8);
        let request = f
            .hub
            .register_pending(&f.turn, f.pending("rm -rf build"))
            .await
            .unwrap();
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, Some(0), None).await.unwrap();

        // Assert
        let (method, params) = next_params(&mut outbox).await;
        assert_eq!(method, "session.event");
        assert_eq!(params["kind"], PERMISSION_REQUEST_KIND);
        let (method, params) = next_params(&mut outbox).await;
        assert_eq!(method, PERMISSION_REQUEST_NOTIFICATION);
        assert_eq!(params["request_id"], request.as_str());
        assert_eq!(params["command"], "rm -rf build");
        assert_eq!(params["tool"], "bash");
    }

    #[tokio::test]
    async fn resolved_request_is_not_resent() {
        // Arrange
        let f = fixture(8);
        let request = f
            .hub
            .register_pending(&f.turn, f.pending("ls"))
            .await
            .unwrap();
        f.hub
            .resolve_pending(&request, PermissionDecision::AllowOnce)
            .await
            .unwrap();
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, Some(1), None).await.unwrap();

        // Assert
        assert_quiet(&mut outbox).await;
    }

    #[tokio::test]
    async fn sessions_do_not_cross_streams() {
        // Arrange
        let f = fixture(8);
        let (other, other_turn) = new_session(&mut f.hub.store(), "/other");
        let (conn, mut outbox) = f.hub.connect();
        conn.subscribe(&f.session, Some(0), None).await.unwrap();

        // Act
        f.hub
            .publish(&other, &other_turn, record("elsewhere"))
            .await
            .unwrap();
        f.publish(1).await;

        // Assert
        let (_, params) = next_params(&mut outbox).await;
        assert_eq!(params["session_id"], f.session.as_str());
        assert_quiet(&mut outbox).await;
    }

    #[tokio::test]
    async fn resume_longer_than_the_queue_keeps_a_steady_reader_connected() {
        // Arrange
        let f = fixture(4);
        f.publish_many(20).await;
        let (conn, mut outbox) = f.hub.connect();
        let reader = tokio::spawn(async move {
            let mut seqs = Vec::new();
            while seqs.len() < 22 {
                seqs.push(next_seq(&mut outbox).await);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            (seqs, outbox)
        });

        // Act
        conn.subscribe(&f.session, Some(0), None).await.unwrap();
        f.publish(21).await;
        f.publish(22).await;

        // Assert
        let (seqs, outbox) = reader.await.unwrap();
        assert_eq!(seqs, (1..=22).collect::<Vec<u64>>());
        let kicked = tokio::time::timeout(QUIET, outbox.kicked.notified()).await;
        assert!(kicked.is_err(), "a reading client was disconnected");
    }

    #[tokio::test]
    async fn resume_with_a_dead_connection_ends_instead_of_waiting_forever() {
        // Arrange
        let f = fixture(2);
        f.publish_many(10).await;
        let (conn, outbox) = f.hub.connect();
        drop(outbox);

        // Act
        let result = tokio::time::timeout(RECV_TIMEOUT, conn.subscribe(&f.session, Some(0), None))
            .await
            .expect("subscribe hung");

        // Assert
        assert!(result.is_ok());
        assert!(f.hub.sessions().is_empty());
    }

    #[tokio::test]
    async fn acknowledgement_is_queued_before_the_replayed_events() {
        // Arrange
        let f = fixture(8);
        f.publish_many(2).await;
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, Some(0), Some(b"ack".to_vec()))
            .await
            .unwrap();

        // Assert
        let first = outbox.events.recv().await.unwrap();
        assert_eq!(first, b"ack");
        assert_eq!(next_seq(&mut outbox).await, 1);
        assert_eq!(next_seq(&mut outbox).await, 2);
    }

    #[tokio::test]
    async fn subscribing_to_an_unknown_session_queues_no_acknowledgement() {
        // Arrange
        let f = fixture(8);
        let (conn, mut outbox) = f.hub.connect();

        // Act
        let result = conn
            .subscribe(&SessionId::new("404"), Some(0), Some(b"ack".to_vec()))
            .await;

        // Assert
        assert!(result.is_err());
        assert_quiet(&mut outbox).await;
    }

    #[tokio::test]
    async fn omitted_cursor_skips_history_and_follows_live() {
        // Arrange
        let f = fixture(8);
        f.publish_many(3).await;
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, None, None).await.unwrap();
        assert_quiet(&mut outbox).await;
        f.publish(4).await;

        // Assert
        assert_eq!(next_seq(&mut outbox).await, 4);
    }

    #[tokio::test]
    async fn replay_longer_than_one_page_arrives_complete_and_in_order() {
        // Arrange
        let f = fixture_paged(16, 2);
        f.publish_many(5).await;
        let (conn, mut outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, Some(0), None).await.unwrap();

        // Assert
        for expected in 1..=5 {
            assert_eq!(next_seq(&mut outbox).await, expected);
        }
        assert_quiet(&mut outbox).await;
    }

    #[tokio::test]
    async fn backlog_that_overflows_during_replay_disconnects_the_subscriber() {
        // Arrange
        let f = fixture(2);
        f.race_publish_many_at(Stage::Registered, 3);
        let (conn, outbox) = f.hub.connect();

        // Act
        conn.subscribe(&f.session, None, None).await.unwrap();

        // Assert
        tokio::time::timeout(RECV_TIMEOUT, outbox.kicked.notified())
            .await
            .expect("the subscriber was not disconnected");
        assert!(f.hub.sessions().is_empty());
    }

    #[tokio::test]
    async fn live_subscribers_receive_the_permission_request_notification() {
        // Arrange
        let f = fixture(8);
        let (conn, mut outbox) = f.hub.connect();
        conn.subscribe(&f.session, None, None).await.unwrap();

        // Act
        let request = f
            .hub
            .register_pending(&f.turn, f.pending("rm -rf build"))
            .await
            .unwrap();

        // Assert
        let (method, params) = next_params(&mut outbox).await;
        assert_eq!(method, "session.event");
        assert_eq!(params["kind"], PERMISSION_REQUEST_KIND);
        let (method, params) = next_params(&mut outbox).await;
        assert_eq!(method, PERMISSION_REQUEST_NOTIFICATION);
        assert_eq!(params["request_id"], request.as_str());
        assert_eq!(params["command"], "rm -rf build");
    }

    #[tokio::test]
    async fn pending_request_for_an_unknown_turn_leaves_nothing_behind() {
        // Arrange
        let f = fixture(8);

        // Act
        let result = f
            .hub
            .register_pending(&TurnId::new("999"), f.pending("ls"))
            .await;

        // Assert
        assert!(matches!(
            result,
            Err(HubError::Store(StoreError::TurnNotFound { .. }))
        ));
        let open = f.hub.store().list_pending(&f.session).unwrap();
        assert_eq!(open.len(), 0);
    }
}
