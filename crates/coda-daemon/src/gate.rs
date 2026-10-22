// SPDX-License-Identifier: AGPL-3.0-only

//! Wires sessions to the permission broker: one broker per session, and a gate
//! that announces its prompts to subscribers.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use coda_core::broker::{
    Broker, BrokerConfig, BrokerError, PendingNotice, RunGrants, resolve_repo,
};
use coda_protocol::events::{Event, TIMEOUT_KIND};
use coda_protocol::methods::PermissionRespondParams;
use coda_protocol::types::{PermissionDecision, RequestId, SessionId, TurnId};
use coda_providers::gate::PermissionGate;
use coda_providers::tool_request::ToolRequest;
use coda_store::{EventRecord, NewPending, SessionRecord};
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::subscribe::Hub;
use crate::turn::permission_mode;

pub use coda_core::broker::PERMISSION_TIMEOUT;

/// The brokers of the sessions the daemon serves.
pub struct Brokers {
    hub: Arc<Hub>,
    /// `tool:pattern` grants from the config, stored for every session's repo.
    grants: Vec<String>,
    /// Never held across an await.
    by_session: Mutex<HashMap<SessionId, Arc<Broker>>>,
}

impl Brokers {
    #[must_use]
    pub fn new(hub: Arc<Hub>, grants: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            hub,
            grants,
            by_session: Mutex::default(),
        })
    }

    /// The broker of `record`'s session, created on first use. Creating it
    /// denies the prompts a previous run left unanswered.
    ///
    /// # Errors
    /// Returns [`BrokerError`] when a configured grant is malformed or the
    /// store refuses.
    pub async fn broker(
        &self,
        record: &SessionRecord,
        prompt_timeout: Duration,
    ) -> Result<Arc<Broker>, BrokerError> {
        if let Some(broker) = self.known(&record.id) {
            return Ok(broker);
        }
        let cwd = PathBuf::from(&record.cwd);
        let broker = Arc::new(
            Broker::new(
                self.hub.store_handle(),
                BrokerConfig {
                    session: record.id.clone(),
                    repo: resolve_repo(&cwd),
                    workspace_root: cwd.clone(),
                    cwd,
                    mode: permission_mode(record.mode),
                    prompt_timeout,
                    grants: self.grants.clone(),
                },
            )
            .await?,
        );
        // A concurrent first use may have won; keep its broker so prompts share one waiter table.
        Ok(Arc::clone(
            self.map()
                .entry(record.id.clone())
                .or_insert_with(|| broker),
        ))
    }

    /// Hands a client's answer to the broker waiting for it; `false` when the
    /// request is unknown, already answered, or belongs to another session.
    pub async fn respond(&self, params: &PermissionRespondParams) -> bool {
        let Some(broker) = self.known(&params.session_id) else {
            warn!(session = %params.session_id, "permission answer for a session with no broker");
            return false;
        };
        let owned = broker
            .pending()
            .await
            .is_ok_and(|open| open.iter().any(|p| p.id == params.request_id));
        if !owned {
            warn!(
                session = %params.session_id,
                request = %params.request_id,
                "permission answer names no open request of this session"
            );
            return false;
        }
        match broker.answer(&params.request_id, params.decision).await {
            Ok(()) => true,
            Err(e) => {
                warn!(request = %params.request_id, error = %e, "permission answer not accepted");
                false
            }
        }
    }

    fn known(&self, session: &SessionId) -> Option<Arc<Broker>> {
        self.map().get(session).cloned()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Arc<Broker>>> {
        self.by_session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// A broker whose prompts are also announced to the session's subscribers,
/// under the turn that is current when they start waiting.
pub struct BrokerGate {
    broker: Arc<Broker>,
    announcer: JoinHandle<()>,
    hub: Arc<Hub>,
    session: SessionId,
    turn: Arc<Mutex<TurnId>>,
}

impl BrokerGate {
    #[must_use]
    pub fn new(
        hub: &Arc<Hub>,
        broker: Arc<Broker>,
        record: &SessionRecord,
        turn: Arc<Mutex<TurnId>>,
    ) -> Self {
        let announce = Announce {
            hub: Arc::clone(hub),
            broker: Arc::clone(&broker),
            session: record.id.clone(),
            repo: resolve_repo(&PathBuf::from(&record.cwd))
                .to_string_lossy()
                .into_owned(),
            turn: Arc::clone(&turn),
        };
        // Subscribed before the task starts, so no prompt is missed.
        let (notices, expired) = (broker.subscribe(), broker.subscribe_expired());
        Self {
            announcer: tokio::spawn(announce.run(notices, expired)),
            broker,
            hub: Arc::clone(hub),
            session: record.id.clone(),
            turn,
        }
    }
}

impl Drop for BrokerGate {
    fn drop(&mut self) {
        self.announcer.abort();
    }
}

impl BrokerGate {
    /// Decides a call made by the sub-agent `agent` of this session. The prompt
    /// is announced like the session's own, with the agent's name on it, and an
    /// "always allow" answer is kept in the run's `grants`, not stored.
    pub async fn request_for_agent(
        &self,
        request: &ToolRequest,
        agent: &str,
        grants: &RunGrants,
    ) -> PermissionDecision {
        Watched {
            inner: Box::pin(self.broker.request_for_agent(request, agent, grants)),
            gate: self,
            done: false,
        }
        .await
    }
}

impl PermissionGate for BrokerGate {
    async fn request(&self, request: &ToolRequest) -> PermissionDecision {
        Watched {
            inner: Box::pin(self.broker.request(request)),
            gate: self,
            done: false,
        }
        .await
    }
}

/// A request that tells subscribers about its prompt when it is dropped
/// unanswered, for example by a cancelled turn.
struct Watched<'a, F> {
    inner: Pin<Box<F>>,
    gate: &'a BrokerGate,
    done: bool,
}

impl<F: Future<Output = PermissionDecision>> Future for Watched<'_, F> {
    type Output = PermissionDecision;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let polled = self.inner.as_mut().poll(cx);
        self.done = polled.is_ready();
        polled
    }
}

impl<F> Drop for Watched<'_, F> {
    // Runs before `inner` is dropped, while the broker has not yet denied the prompt.
    fn drop(&mut self) {
        if !self.done {
            self.gate.record_abandoned();
        }
    }
}

impl BrokerGate {
    /// Writes a timeout event for each prompt still open. Blocking, on purpose:
    /// the broker denies the rows as soon as the dropped request is released,
    /// and the events must precede the end of the turn.
    fn record_abandoned(&self) {
        let open = lock(&self.hub.store_handle()).list_pending(&self.session);
        let turn = lock(&self.turn).clone();
        for pending in open.into_iter().flatten() {
            let Some(record) = timeout_record(&self.session, &pending.id, pending.tool) else {
                continue;
            };
            if let Err(e) = self.hub.append_and_fan_out(&self.session, &turn, &record) {
                warn!(session = %self.session, error = %e, "could not record an abandoned prompt");
            }
        }
    }
}

fn lock<T>(cell: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    cell.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The timeout event of a prompt; `None` (logged) when it cannot be serialized.
fn timeout_record(
    session: &SessionId,
    request_id: &RequestId,
    tool: String,
) -> Option<EventRecord> {
    let event = Event::Timeout {
        session_id: session.clone(),
        request_id: request_id.clone(),
        tool,
    };
    match serde_json::to_string(&event) {
        Ok(body) => Some(EventRecord {
            kind: TIMEOUT_KIND.to_owned(),
            body,
        }),
        Err(e) => {
            warn!(session = %session, request = %request_id, error = %e, "timeout event not recorded");
            None
        }
    }
}

struct Announce {
    hub: Arc<Hub>,
    broker: Arc<Broker>,
    session: SessionId,
    repo: String,
    turn: Arc<Mutex<TurnId>>,
}

impl Announce {
    async fn run(self, mut notices: Receiver<PendingNotice>, mut expired: Receiver<PendingNotice>) {
        loop {
            tokio::select! {
                notice = notices.recv() => match notice {
                    Ok(notice) => self.announce(notice).await,
                    Err(RecvError::Lagged(missed)) => {
                        warn!(session = %self.session, missed, "missed permission announcements");
                        self.deny_open().await;
                    }
                    Err(RecvError::Closed) => return,
                },
                notice = expired.recv() => match notice {
                    Ok(notice) => self.record_timeout(notice).await,
                    Err(RecvError::Lagged(missed)) => {
                        warn!(session = %self.session, missed, "missed permission timeouts");
                    }
                    Err(RecvError::Closed) => return,
                },
            }
        }
    }

    /// A prompt no client can see would wait out its whole timeout, so it is denied instead.
    async fn announce(&self, notice: PendingNotice) {
        let turn = lock(&self.turn).clone();
        let pending = NewPending {
            session: self.session.clone(),
            tool: notice.tool,
            repo_path: self.repo.clone(),
            command: notice.command,
        };
        if let Err(e) = self.hub.announce_pending(&turn, &notice.id, pending).await {
            warn!(session = %self.session, request = %notice.id, error = %e, "could not announce a permission request; denying it");
            self.deny(&notice.id).await;
        }
    }

    /// Missed announcements cannot be told apart from the open prompts, so all are denied.
    async fn deny_open(&self) {
        match self.broker.pending().await {
            Ok(open) => {
                for notice in open {
                    self.deny(&notice.id).await;
                }
            }
            Err(e) => warn!(session = %self.session, error = %e, "could not list the open prompts"),
        }
    }

    async fn deny(&self, request: &RequestId) {
        if let Err(e) = self.broker.answer(request, PermissionDecision::Deny).await {
            warn!(session = %self.session, request = %request, error = %e, "could not deny an unannounced prompt");
        }
    }

    async fn record_timeout(&self, notice: PendingNotice) {
        let Some(record) = timeout_record(&self.session, &notice.id, notice.tool) else {
            return;
        };
        let turn = lock(&self.turn).clone();
        if let Err(e) = self.hub.publish(&self.session, &turn, record).await {
            warn!(session = %self.session, error = %e, "could not record a prompt timeout");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use coda_core::broker::Broker;
    use coda_protocol::methods::PermissionRespondParams;
    use coda_protocol::paths::{Target, classify_target};
    use coda_protocol::types::{BackendKind, Mode, PermissionDecision, RequestId, TurnId};
    use coda_providers::gate::PermissionGate;
    use coda_providers::policy::{
        ClassifiedTarget, PolicyInput, TableRow, Verdict, decide, decision_table, grant_pattern,
    };
    use coda_providers::tool_request::{Tool, ToolRequest};
    use coda_store::{GrantKey, NewSession, SessionRecord, Store};
    use tempfile::TempDir;
    use tokio::task::JoinHandle;

    use super::{BrokerGate, Brokers};
    use crate::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub, Outbox};

    const SHORT_TIMEOUT: Duration = Duration::from_millis(100);
    const ANSWER_WAIT: Duration = Duration::from_secs(10);
    const ESCAPE_LINK: &str = "link";

    struct Fixture {
        dir: TempDir,
        /// Canonical root holding the `ws` and `other` repositories.
        root: PathBuf,
        hub: Arc<Hub>,
        brokers: Arc<Brokers>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            for repo_dir in ["ws/.git", "ws/x", "other/.git", "other/x"] {
                std::fs::create_dir_all(root.join(repo_dir)).unwrap();
            }
            std::os::unix::fs::symlink("/etc", root.join("ws").join(ESCAPE_LINK)).unwrap();
            let store = Store::open(&dir.path().join("data")).unwrap();
            let hub = Hub::new(store, DEFAULT_QUEUE_CAPACITY);
            Self {
                brokers: Brokers::new(Arc::clone(&hub), Vec::new()),
                dir,
                root,
                hub,
            }
        }

        /// The real directory standing for a table path such as `/ws/x`.
        fn real(&self, path: &str) -> PathBuf {
            self.root.join(path.trim_start_matches('/'))
        }

        async fn session(&self, cwd: &str, mode: Mode) -> SessionRecord {
            let new = NewSession {
                backend: BackendKind::ClaudeCode,
                model: None,
                cwd: self.real(cwd).to_string_lossy().into_owned(),
                mode,
            };
            let id = self
                .hub
                .with_store_mut(move |store| store.create_session(&new))
                .await
                .unwrap();
            self.hub
                .with_store(move |store| store.load_session(&id))
                .await
                .unwrap()
        }

        async fn broker(&self, cwd: &str, mode: Mode) -> (SessionRecord, Arc<Broker>) {
            self.broker_waiting(cwd, mode, SHORT_TIMEOUT).await
        }

        async fn broker_waiting(
            &self,
            cwd: &str,
            mode: Mode,
            prompt_timeout: Duration,
        ) -> (SessionRecord, Arc<Broker>) {
            let record = self.session(cwd, mode).await;
            let broker = self.brokers.broker(&record, prompt_timeout).await.unwrap();
            (record, broker)
        }

        /// A gate asking for one edit, and the id of the prompt it raises.
        async fn ask(
            &self,
            record: &SessionRecord,
            broker: &Arc<Broker>,
        ) -> (JoinHandle<PermissionDecision>, RequestId) {
            let gate = self.gate(record, broker);
            let mut notices = broker.subscribe();
            let request = edit(self, "src/a.rs", "/ws");
            let asking = tokio::spawn(async move { gate.request(&request).await });
            let notice = tokio::time::timeout(ANSWER_WAIT, notices.recv())
                .await
                .unwrap()
                .unwrap();
            (asking, notice.id)
        }

        async fn answer(
            &self,
            record: &SessionRecord,
            id: &RequestId,
            decision: PermissionDecision,
        ) -> bool {
            self.brokers
                .respond(&PermissionRespondParams {
                    session_id: record.id.clone(),
                    request_id: id.clone(),
                    decision,
                })
                .await
        }

        /// Waits for the first event of `kind` that the session's subscribers get.
        async fn event_of_kind(outbox: &mut Outbox, kind: &str) -> String {
            tokio::time::timeout(ANSWER_WAIT, async {
                loop {
                    let frame =
                        String::from_utf8_lossy(&outbox.events.recv().await.unwrap()).into_owned();
                    if frame.contains(&format!("\"kind\":\"{kind}\"")) {
                        return frame;
                    }
                }
            })
            .await
            .expect("the event never reached the subscriber")
        }

        fn gate(&self, record: &SessionRecord, broker: &Arc<Broker>) -> BrokerGate {
            let turn = self.store().begin_turn(&record.id).unwrap();
            let turn = Arc::new(Mutex::new(turn));
            BrokerGate::new(&self.hub, Arc::clone(broker), record, turn)
        }

        fn store(&self) -> Store {
            Store::open(&self.dir.path().join("data")).unwrap()
        }
    }

    fn edit(fixture: &Fixture, path: &str, cwd: &str) -> ToolRequest {
        ToolRequest {
            tool: Tool::Edit,
            target_paths: vec![PathBuf::from(path)],
            command: None,
            cwd: fixture.real(cwd),
        }
    }

    fn request_of(fixture: &Fixture, row: &TableRow) -> ToolRequest {
        ToolRequest {
            tool: row.tool.clone(),
            target_paths: row
                .targets
                .iter()
                .map(|t| match t.target {
                    Target::Escapes => Path::new(ESCAPE_LINK).join("secret"),
                    _ => t.path.clone(),
                })
                .collect(),
            command: row.command.clone(),
            cwd: fixture.real(&row.cwd.to_string_lossy()),
        }
    }

    /// The verdict of the pre-broker session gate: the shared policy over a
    /// grant looked up by the literal repo, tool, cwd and pattern.
    fn session_gate_verdict(fixture: &Fixture, row: &TableRow) -> Verdict {
        let request = request_of(fixture, row);
        let workspace_root = fixture.real("/ws");
        let targets: Vec<ClassifiedTarget> = request
            .target_paths
            .iter()
            .map(|path| ClassifiedTarget {
                path: path.clone(),
                target: classify_target(&workspace_root, path),
            })
            .collect();
        let pattern = grant_pattern(
            &request.tool,
            request.command.as_deref(),
            &request.target_paths,
        );
        let (repo, cwd) = (
            fixture.real(&row.repo.to_string_lossy()),
            request.cwd.to_string_lossy().into_owned(),
        );
        let granted = fixture
            .store()
            .lookup_grant(&GrantKey {
                repo_path: &repo.to_string_lossy(),
                tool: request.tool.name(),
                cwd: &cwd,
                pattern: &pattern,
            })
            .unwrap()
            .is_some();
        decide(&PolicyInput {
            mode: row.mode,
            tool: &request.tool,
            workspace_root: &workspace_root,
            targets: &targets,
            granted,
        })
    }

    #[tokio::test]
    async fn the_session_gate_and_the_broker_agree_on_every_row_of_the_decision_table() {
        for row in decision_table() {
            // Arrange
            let fixture = Fixture::new();
            if let Some(stored) = &row.stored {
                let (repo, cwd) = (
                    fixture.real(&stored.repo.to_string_lossy()),
                    fixture.real(&stored.cwd.to_string_lossy()),
                );
                fixture
                    .hub
                    .add_grant(
                        repo.to_string_lossy().into_owned(),
                        stored.tool.clone(),
                        cwd.to_string_lossy().into_owned(),
                        stored.pattern.clone(),
                    )
                    .await
                    .unwrap();
            }
            let record = fixture.session("/ws/x", Mode::default()).await;
            let config = coda_core::broker::BrokerConfig {
                session: record.id.clone(),
                repo: fixture.real(&row.repo.to_string_lossy()),
                cwd: fixture.real("/ws/x"),
                workspace_root: fixture.real("/ws"),
                mode: row.mode,
                prompt_timeout: SHORT_TIMEOUT,
                grants: Vec::new(),
            };
            let broker = Broker::new(fixture.hub.store_handle(), config)
                .await
                .unwrap();

            // Act
            let old = session_gate_verdict(&fixture, &row);
            let new = broker.verdict(&request_of(&fixture, &row)).await.unwrap();

            // Assert
            assert_eq!(new, old, "row: {} ({:?})", row.name, row.mode);
            assert_eq!(new, row.expected, "row: {} ({:?})", row.name, row.mode);
        }
    }

    #[tokio::test]
    async fn a_decision_is_recorded_for_the_session_and_survives_a_restart() {
        // Arrange
        let fixture = Fixture::new();
        let (record, broker) = fixture.broker("/ws/x", Mode::AcceptEdits).await;
        let gate = fixture.gate(&record, &broker);

        // Act
        let decision = gate.request(&edit(&fixture, "src/a.rs", "/ws")).await;

        // Assert
        assert_eq!(decision, PermissionDecision::AllowOnce);
        let reopened = fixture.store().list_decisions(&record.id).unwrap();
        assert_eq!(reopened.len(), 1);
        assert!(reopened[0].allowed);
        assert_eq!(reopened[0].repo_path, fixture.real("/ws").to_string_lossy());
    }

    #[tokio::test]
    async fn an_always_allow_reaches_later_sessions_of_the_repo_and_no_other_repo() {
        // Arrange
        let fixture = Fixture::new();
        let (first, broker) = fixture.broker("/ws/x", Mode::Default).await;
        let gate = fixture.gate(&first, &broker);
        let mut notices = broker.subscribe();
        let asking = tokio::spawn({
            let request = edit(&fixture, "src/a.rs", "/ws");
            async move { gate.request(&request).await }
        });
        let notice = tokio::time::timeout(ANSWER_WAIT, notices.recv())
            .await
            .unwrap()
            .unwrap();

        // Act
        broker
            .answer(&notice.id, PermissionDecision::AllowAlways)
            .await
            .unwrap();
        asking.await.unwrap();
        let (_, later) = fixture.broker("/ws/x", Mode::Default).await;
        let (_, elsewhere) = fixture.broker("/other/x", Mode::Default).await;
        let same_repo = later
            .verdict(&edit(&fixture, "src/a.rs", "/ws"))
            .await
            .unwrap();
        let other_repo = elsewhere
            .verdict(&edit(&fixture, "src/a.rs", "/other"))
            .await
            .unwrap();

        // Assert
        assert_eq!(same_repo, Verdict::Allow);
        assert_eq!(other_repo, Verdict::Prompt);
    }

    #[tokio::test]
    async fn a_prompt_is_announced_to_subscribers_and_answered_through_respond() {
        // Arrange
        let fixture = Fixture::new();
        let (record, broker) = fixture.broker("/ws/x", Mode::Default).await;
        let gate = fixture.gate(&record, &broker);
        let (conn, mut outbox) = fixture.hub.connect();
        conn.subscribe(&record.id, Some(0), None).await.unwrap();
        let mut notices = broker.subscribe();
        let asking = tokio::spawn({
            let request = edit(&fixture, "src/a.rs", "/ws");
            async move { gate.request(&request).await }
        });
        let notice = tokio::time::timeout(ANSWER_WAIT, notices.recv())
            .await
            .unwrap()
            .unwrap();

        // Act
        let announced = tokio::time::timeout(ANSWER_WAIT, async {
            loop {
                let frame = outbox.events.recv().await.unwrap();
                if String::from_utf8_lossy(&frame).contains("permission.request") {
                    return frame;
                }
            }
        })
        .await
        .unwrap();
        let answered = fixture
            .brokers
            .respond(&PermissionRespondParams {
                session_id: record.id.clone(),
                request_id: notice.id.clone(),
                decision: PermissionDecision::AllowOnce,
            })
            .await;

        // Assert
        assert!(String::from_utf8_lossy(&announced).contains(notice.id.as_str()));
        assert!(answered);
        assert_eq!(asking.await.unwrap(), PermissionDecision::AllowOnce);
    }

    #[tokio::test]
    async fn an_answer_naming_another_sessions_request_is_refused() {
        // Arrange
        let fixture = Fixture::new();
        let (record, broker) = fixture.broker("/ws/x", Mode::Default).await;
        let (stranger, _) = fixture.broker("/other/x", Mode::Default).await;
        let gate = fixture.gate(&record, &broker);
        let mut notices = broker.subscribe();
        let asking = tokio::spawn({
            let request = edit(&fixture, "src/a.rs", "/ws");
            async move { gate.request(&request).await }
        });
        let notice = tokio::time::timeout(ANSWER_WAIT, notices.recv())
            .await
            .unwrap()
            .unwrap();

        // Act
        let answered = fixture
            .brokers
            .respond(&PermissionRespondParams {
                session_id: stranger.id.clone(),
                request_id: notice.id.clone(),
                decision: PermissionDecision::AllowAlways,
            })
            .await;

        // Assert
        assert!(!answered);
        assert_eq!(asking.await.unwrap(), PermissionDecision::Deny);
    }

    #[tokio::test]
    async fn a_session_keeps_one_broker() {
        // Arrange
        let fixture = Fixture::new();
        let (record, first) = fixture.broker("/ws/x", Mode::Default).await;

        // Act
        let second = fixture
            .brokers
            .broker(&record, SHORT_TIMEOUT)
            .await
            .unwrap();

        // Assert
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn a_prompt_left_by_a_previous_run_is_denied_when_the_broker_starts() {
        // Arrange
        let fixture = Fixture::new();
        let record = fixture.session("/ws/x", Mode::Default).await;
        let id = record.id.clone();
        fixture
            .hub
            .with_store_mut(move |store| {
                store.add_pending(&coda_store::NewPending {
                    session: id,
                    tool: "bash".to_owned(),
                    repo_path: "/r".to_owned(),
                    command: "ls".to_owned(),
                })
            })
            .await
            .unwrap();

        // Act
        let broker = fixture
            .brokers
            .broker(&record, Duration::ZERO)
            .await
            .unwrap();

        // Assert
        assert_eq!(broker.pending().await.unwrap(), Vec::new());
    }

    #[tokio::test]
    async fn a_prompt_that_cannot_be_announced_is_denied_at_once() {
        // Arrange
        let fixture = Fixture::new();
        let (record, broker) = fixture
            .broker_waiting("/ws/x", Mode::Default, ANSWER_WAIT)
            .await;
        let gate = fixture.gate(&record, &broker);
        *gate.turn.lock().unwrap() = TurnId::new("999");

        // Act
        let decision = tokio::time::timeout(
            Duration::from_secs(5),
            gate.request(&edit(&fixture, "src/a.rs", "/ws")),
        )
        .await
        .expect("an invisible prompt kept waiting");

        // Assert
        assert_eq!(decision, PermissionDecision::Deny);
        assert_eq!(broker.pending().await.unwrap().len(), 0);
        let recorded = fixture.store().list_decisions(&record.id).unwrap();
        assert!(!recorded[0].allowed);
    }

    #[tokio::test]
    async fn an_answer_after_the_deadline_is_refused_and_the_denial_stands() {
        // Arrange
        let fixture = Fixture::new();
        let (record, broker) = fixture.broker("/ws/x", Mode::Default).await;
        let (asking, id) = fixture.ask(&record, &broker).await;
        let decision = asking.await.unwrap();

        // Act
        let accepted = fixture
            .answer(&record, &id, PermissionDecision::AllowAlways)
            .await;

        // Assert
        assert_eq!(decision, PermissionDecision::Deny);
        assert!(!accepted);
        let recorded = fixture.store().list_decisions(&record.id).unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(!recorded[0].allowed);
        let later = broker.verdict(&edit(&fixture, "src/a.rs", "/ws")).await;
        assert_eq!(later.unwrap(), Verdict::Prompt);
    }

    #[tokio::test]
    async fn a_duplicate_or_stale_answer_is_refused_without_changing_the_outcome() {
        // Arrange
        let fixture = Fixture::new();
        let (record, broker) = fixture
            .broker_waiting("/ws/x", Mode::Default, ANSWER_WAIT)
            .await;
        let (asking, id) = fixture.ask(&record, &broker).await;
        let first = fixture
            .answer(&record, &id, PermissionDecision::AllowOnce)
            .await;

        // Act
        let duplicate = fixture.answer(&record, &id, PermissionDecision::Deny).await;
        let stale = fixture
            .answer(
                &record,
                &RequestId::new("424242"),
                PermissionDecision::AllowOnce,
            )
            .await;

        // Assert
        assert!(first);
        assert!(!duplicate);
        assert!(!stale);
        assert_eq!(asking.await.unwrap(), PermissionDecision::AllowOnce);
        let recorded = fixture.store().list_decisions(&record.id).unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].allowed);
    }

    #[tokio::test]
    async fn a_timeout_with_a_silent_client_denies_and_records_the_timeout_event() {
        // Arrange
        let fixture = Fixture::new();
        let (record, broker) = fixture.broker("/ws/x", Mode::Default).await;
        let (conn, mut outbox) = fixture.hub.connect();
        conn.subscribe(&record.id, Some(0), None).await.unwrap();

        let gate = fixture.gate(&record, &broker);
        let mut notices = broker.subscribe();

        // Act
        let decision = gate.request(&edit(&fixture, "src/a.rs", "/ws")).await;

        // Assert
        assert_eq!(decision, PermissionDecision::Deny);
        let id = notices.recv().await.unwrap().id;
        let event = Fixture::event_of_kind(&mut outbox, "timeout").await;
        assert!(event.contains(id.as_str()));
        assert_eq!(broker.pending().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_dropped_prompt_records_the_timeout_event_and_denies_the_row() {
        // Arrange
        let fixture = Fixture::new();
        let (record, broker) = fixture
            .broker_waiting("/ws/x", Mode::Default, ANSWER_WAIT)
            .await;
        let (conn, mut outbox) = fixture.hub.connect();
        conn.subscribe(&record.id, Some(0), None).await.unwrap();
        let (asking, id) = fixture.ask(&record, &broker).await;

        // Act
        asking.abort();
        let _ = asking.await;

        // Assert
        let event = Fixture::event_of_kind(&mut outbox, "timeout").await;
        assert!(event.contains(id.as_str()));
        assert_eq!(broker.pending().await.unwrap().len(), 0);
    }
}
