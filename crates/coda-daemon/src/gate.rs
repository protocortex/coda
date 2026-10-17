// SPDX-License-Identifier: AGPL-3.0-only

//! The session permission gate: applies the shared policy with the stored
//! per-repo grants, and waits for a client on a prompt verdict.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use coda_config::config::PermissionMode;
use coda_protocol::paths::classify_target;
use coda_protocol::types::{PermissionDecision, RequestId, SessionId, TurnId};
use coda_providers::gate::PermissionGate;
use coda_providers::policy::{ClassifiedTarget, PolicyInput, Verdict, decide, grant_pattern};
use coda_providers::tool_request::ToolRequest;
use coda_store::{GrantKey, NewDecision, NewPending};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::subscribe::{Hub, HubError};

/// How long a prompt waits for an answer before it is denied.
pub const PERMISSION_TIMEOUT: Duration = Duration::from_mins(5);
/// How long a timed-out prompt waits for an answer that is already being stored.
const ANSWER_HANDOFF: Duration = Duration::from_secs(1);

/// What the gate needs to know about the turn it guards.
#[derive(Debug, Clone)]
pub struct GateConfig {
    pub session: SessionId,
    pub turn: TurnId,
    /// Canonical git root, the repo identity of grants.
    pub repo: PathBuf,
    pub workspace_root: PathBuf,
    pub mode: PermissionMode,
    pub prompt_timeout: Duration,
}

/// Why the gate could not reach a verdict.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    #[error("the permission store failed: {0}")]
    Store(#[from] HubError),
}

/// Hands client answers to the gates waiting for them.
#[derive(Debug)]
pub struct PermissionAnswers {
    slots: Mutex<HashMap<RequestId, Slot>>,
    /// How long an answer nobody collected is kept.
    stale_after: Duration,
}

#[derive(Debug)]
enum Slot {
    Waiting(oneshot::Sender<PermissionDecision>),
    Answered(PermissionDecision, Instant),
}

impl PermissionAnswers {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: Mutex::default(),
            stale_after: ANSWER_HANDOFF,
        })
    }

    /// Records a client's answer and wakes the gate waiting on it.
    ///
    /// # Errors
    /// Returns [`GateError::Store`] for an unknown or already answered request.
    pub async fn answer(
        &self,
        hub: &Arc<Hub>,
        request: &RequestId,
        decision: PermissionDecision,
    ) -> Result<(), GateError> {
        hub.resolve_pending(request, decision).await?;
        let slot = self.slots().remove(request);
        if let Some(Slot::Waiting(tx)) = slot {
            // The gate may have timed out and gone away; the stored answer stands.
            let _ = tx.send(decision);
        } else {
            // Answered before the gate started waiting. A gate that already
            // gave up never collects it, so older answers are dropped.
            let mut slots = self.slots();
            let stale_after = self.stale_after;
            slots.retain(|_, slot| match slot {
                Slot::Answered(_, at) => at.elapsed() < stale_after,
                Slot::Waiting(_) => true,
            });
            slots.insert(request.clone(), Slot::Answered(decision, Instant::now()));
        }
        Ok(())
    }

    fn wait_for(&self, request: &RequestId) -> Wait<'_> {
        let mut slots = self.slots();
        if let Some(Slot::Answered(decision, _)) = slots.remove(request) {
            return Wait::Ready(decision);
        }
        let (tx, rx) = oneshot::channel();
        slots.insert(request.clone(), Slot::Waiting(tx));
        // Built last: its drop locks the slots this function still holds.
        let guard = SlotGuard {
            answers: self,
            request: request.clone(),
        };
        Wait::Pending(rx, guard)
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<RequestId, Slot>> {
        // Plain data, so a panic elsewhere cannot leave it invalid.
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

enum Wait<'a> {
    Ready(PermissionDecision),
    Pending(oneshot::Receiver<PermissionDecision>, SlotGuard<'a>),
}

/// Frees a request's slot when its gate stops waiting, however it stops.
struct SlotGuard<'a> {
    answers: &'a PermissionAnswers,
    request: RequestId,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.answers.slots().remove(&self.request);
    }
}

/// Decides the permission requests of one turn.
pub struct SessionGate {
    hub: Arc<Hub>,
    config: GateConfig,
    answers: Arc<PermissionAnswers>,
}

impl SessionGate {
    #[must_use]
    pub fn new(hub: Arc<Hub>, config: GateConfig, answers: Arc<PermissionAnswers>) -> Self {
        Self {
            hub,
            config,
            answers,
        }
    }

    /// Applies the policy to `request` without waiting for anyone.
    ///
    /// # Errors
    /// Returns [`GateError::Store`] when the grant lookup fails.
    pub async fn verdict(&self, request: &ToolRequest) -> Result<Verdict, GateError> {
        let targets: Vec<ClassifiedTarget> = request
            .target_paths
            .iter()
            .map(|path| ClassifiedTarget {
                path: path.clone(),
                target: classify_target(&self.config.workspace_root, path),
            })
            .collect();
        let granted = self.is_granted(request).await?;
        Ok(decide(&PolicyInput {
            mode: self.config.mode,
            tool: &request.tool,
            workspace_root: &self.config.workspace_root,
            targets: &targets,
            granted,
        }))
    }

    async fn is_granted(&self, request: &ToolRequest) -> Result<bool, GateError> {
        let key = self.grant_fields(request);
        let found = self
            .hub
            .with_store(move |store| {
                store.lookup_grant(&GrantKey {
                    repo_path: &key.0,
                    tool: &key.1,
                    cwd: &key.2,
                    pattern: &key.3,
                })
            })
            .await?;
        Ok(found.is_some())
    }

    /// Repo, tool, cwd and pattern, the exact fields a grant is keyed by.
    fn grant_fields(&self, request: &ToolRequest) -> (String, String, String, String) {
        (
            self.config.repo.to_string_lossy().into_owned(),
            request.tool.name().to_owned(),
            request.cwd.to_string_lossy().into_owned(),
            grant_pattern(
                &request.tool,
                request.command.as_deref(),
                &request.target_paths,
            ),
        )
    }

    async fn decide_request(&self, request: &ToolRequest) -> Result<PermissionDecision, GateError> {
        let decision = match self.verdict(request).await? {
            Verdict::Allow => PermissionDecision::AllowOnce,
            Verdict::Deny => PermissionDecision::Deny,
            Verdict::Prompt => return self.prompt(request).await,
        };
        self.record_decision(request, decision).await?;
        Ok(decision)
    }

    async fn record_decision(
        &self,
        request: &ToolRequest,
        decision: PermissionDecision,
    ) -> Result<(), GateError> {
        self.hub
            .record_decision(NewDecision {
                session: self.config.session.clone(),
                repo_path: self.config.repo.to_string_lossy().into_owned(),
                command: describe(request),
                decision,
            })
            .await?;
        Ok(())
    }

    /// Queues the request, then waits for a client or the timeout; the timeout denies.
    async fn prompt(&self, request: &ToolRequest) -> Result<PermissionDecision, GateError> {
        let id = self
            .hub
            .register_pending(
                &self.config.turn,
                NewPending {
                    session: self.config.session.clone(),
                    tool: request.tool.name().to_owned(),
                    repo_path: self.config.repo.to_string_lossy().into_owned(),
                    command: describe(request),
                },
            )
            .await?;
        let decision = match self.answers.wait_for(&id) {
            Wait::Ready(decision) => decision,
            Wait::Pending(mut rx, _slot) => {
                match timeout(self.config.prompt_timeout, &mut rx).await {
                    Ok(answer) => answer.unwrap_or(PermissionDecision::Deny),
                    Err(_) => self.deny_unanswered(request, &id, &mut rx).await,
                }
            }
        };
        if decision == PermissionDecision::AllowAlways {
            let (repo, tool, cwd, pattern) = self.grant_fields(request);
            self.hub.add_grant(repo, tool, cwd, pattern).await?;
        }
        Ok(decision)
    }

    async fn deny_unanswered(
        &self,
        request: &ToolRequest,
        id: &RequestId,
        rx: &mut oneshot::Receiver<PermissionDecision>,
    ) -> PermissionDecision {
        if self
            .hub
            .resolve_pending(id, PermissionDecision::Deny)
            .await
            .is_ok()
        {
            return PermissionDecision::Deny;
        }
        // A client answered first; its answer is on the way.
        if let Ok(Ok(decision)) = timeout(ANSWER_HANDOFF, rx).await {
            return decision;
        }
        // The stored answer never reached the gate, so the denial is what applied.
        if let Err(e) = self
            .record_decision(request, PermissionDecision::Deny)
            .await
        {
            tracing::error!(
                session = %self.config.session,
                error = %e,
                "could not record the denial of a late answer"
            );
        }
        PermissionDecision::Deny
    }
}

/// The text a decision is recorded under: the command, or the tool name.
fn describe(request: &ToolRequest) -> String {
    request
        .command
        .clone()
        .unwrap_or_else(|| request.tool.name().to_owned())
}

impl PermissionGate for SessionGate {
    async fn request(&self, request: &ToolRequest) -> PermissionDecision {
        match self.decide_request(request).await {
            Ok(decision) => decision,
            Err(e) => {
                tracing::error!(
                    session = %self.config.session,
                    error = %e,
                    "permission gate failed; denying the request"
                );
                PermissionDecision::Deny
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use coda_config::config::PermissionMode;
    use coda_protocol::paths::Target;
    use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId, TurnId};
    use coda_providers::gate::PermissionGate;
    use coda_providers::policy::{TableRow, Verdict, decision_table};
    use coda_providers::tool_request::{Tool, ToolRequest};
    use coda_store::{GrantKey, NewPending, NewSession, Store};
    use tempfile::TempDir;

    use super::{GateConfig, PermissionAnswers, SessionGate, Slot, Wait};
    use crate::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};

    const SHORT_TIMEOUT: Duration = Duration::from_millis(100);
    const ANSWER_WAIT: Duration = Duration::from_secs(10);
    const POLL: Duration = Duration::from_millis(10);
    const ESCAPE_LINK: &str = "link";

    struct Fixture {
        dir: TempDir,
        hub: Arc<Hub>,
        session: SessionId,
        turn: TurnId,
        answers: Arc<PermissionAnswers>,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut store = Store::open(&dir.path().join("data")).unwrap();
            let session = store
                .create_session(&NewSession {
                    backend: BackendKind::ClaudeCode,
                    model: None,
                    cwd: "/ws".to_owned(),
                    mode: Mode::default(),
                })
                .unwrap();
            let turn = store.begin_turn(&session).unwrap();
            std::fs::create_dir(dir.path().join("ws")).unwrap();
            std::os::unix::fs::symlink("/etc", dir.path().join("ws").join(ESCAPE_LINK)).unwrap();
            Self {
                hub: Hub::new(store, DEFAULT_QUEUE_CAPACITY),
                dir,
                session,
                turn,
                answers: PermissionAnswers::new(),
            }
        }

        fn workspace(&self) -> PathBuf {
            self.dir.path().join("ws")
        }

        fn gate(&self, repo: &str, mode: PermissionMode, timeout: Duration) -> SessionGate {
            SessionGate::new(
                Arc::clone(&self.hub),
                GateConfig {
                    session: self.session.clone(),
                    turn: self.turn.clone(),
                    repo: PathBuf::from(repo),
                    workspace_root: self.workspace(),
                    mode,
                    prompt_timeout: timeout,
                },
                Arc::clone(&self.answers),
            )
        }

        fn store(&self) -> Store {
            Store::open(&self.dir.path().join("data")).unwrap()
        }
    }

    fn bash(command: &str) -> ToolRequest {
        ToolRequest {
            tool: Tool::Bash,
            target_paths: Vec::new(),
            command: Some(command.to_owned()),
            cwd: PathBuf::from("/ws/x"),
        }
    }

    fn request_of(row: &TableRow) -> ToolRequest {
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
            cwd: row.cwd.clone(),
        }
    }

    #[tokio::test]
    async fn the_gate_passes_the_shared_decision_table() {
        // Arrange
        let rows: Vec<TableRow> = decision_table()
            .into_iter()
            .filter(|r| !r.pending)
            .collect();

        for row in rows {
            let fixture = Fixture::new();
            if let Some(stored) = &row.stored {
                fixture
                    .hub
                    .add_grant(
                        stored.repo.to_string_lossy().into_owned(),
                        stored.tool.clone(),
                        stored.cwd.to_string_lossy().into_owned(),
                        stored.pattern.clone(),
                    )
                    .await
                    .unwrap();
            }
            let gate = fixture.gate(&row.repo.to_string_lossy(), row.mode, SHORT_TIMEOUT);

            // Act
            let verdict = gate.verdict(&request_of(&row)).await.unwrap();

            // Assert
            assert_eq!(verdict, row.expected, "row: {} ({:?})", row.name, row.mode);
        }
    }

    #[tokio::test]
    async fn a_grant_of_cargo_test_does_not_cover_a_chained_command() {
        // Arrange
        let fixture = Fixture::new();
        fixture
            .hub
            .add_grant(
                "/ws".into(),
                "bash".into(),
                "/ws/x".into(),
                "cargo test".into(),
            )
            .await
            .unwrap();
        let gate = fixture.gate("/ws", PermissionMode::Prompt, SHORT_TIMEOUT);

        // Act
        let exact = gate.verdict(&bash("cargo test")).await.unwrap();
        let chained = gate.verdict(&bash("cargo test; rm -rf x")).await.unwrap();

        // Assert
        assert_eq!(exact, Verdict::Allow);
        assert_eq!(chained, Verdict::Prompt);
    }

    #[tokio::test]
    async fn an_edit_grant_covers_only_the_exact_file() {
        // Arrange
        let fixture = Fixture::new();
        let edit = |path: &str| ToolRequest {
            tool: Tool::Edit,
            target_paths: vec![PathBuf::from(path)],
            command: None,
            cwd: PathBuf::from("/ws"),
        };
        fixture
            .hub
            .add_grant("/ws".into(), "edit".into(), "/ws".into(), "src/a.rs".into())
            .await
            .unwrap();
        let gate = fixture.gate("/ws", PermissionMode::Prompt, SHORT_TIMEOUT);

        // Act
        let same = gate.verdict(&edit("src/a.rs")).await.unwrap();
        let other = gate.verdict(&edit("src/b.rs")).await.unwrap();
        let manifest = gate.verdict(&edit("Cargo.toml")).await.unwrap();

        // Assert
        assert_eq!(same, Verdict::Allow);
        assert_eq!(other, Verdict::Prompt);
        assert_eq!(manifest, Verdict::Prompt);
    }

    #[tokio::test]
    async fn automatic_verdicts_are_answered_and_recorded() {
        // Arrange
        let fixture = Fixture::new();
        let read = |path: &str| ToolRequest {
            tool: Tool::Read,
            target_paths: vec![PathBuf::from(path)],
            command: None,
            cwd: PathBuf::from("/ws"),
        };
        let gate = fixture.gate("/ws", PermissionMode::Prompt, SHORT_TIMEOUT);

        // Act
        let inside = gate.request(&read("src/a.rs")).await;
        let outside = gate.request(&read("/etc/passwd")).await;

        // Assert
        assert_eq!(inside, PermissionDecision::AllowOnce);
        assert_eq!(outside, PermissionDecision::Deny);
        let recorded: Vec<bool> = fixture
            .store()
            .list_decisions(&fixture.session)
            .unwrap()
            .iter()
            .map(|d| d.allowed)
            .collect();
        assert_eq!(recorded, [true, false]);
    }

    #[tokio::test]
    async fn a_prompt_returns_the_clients_answer_and_records_it_once() {
        // Arrange
        let fixture = Fixture::new();
        let gate = fixture.gate("/ws", PermissionMode::Prompt, ANSWER_WAIT);
        let request = bash("cargo build");

        // Act
        let (decision, ()) = tokio::join!(gate.request(&request), async {
            let id = loop {
                let pending = fixture.store().list_pending(&fixture.session).unwrap();
                if let Some(first) = pending.first() {
                    break first.id.clone();
                }
                tokio::time::sleep(POLL).await;
            };
            fixture
                .answers
                .answer(&fixture.hub, &id, PermissionDecision::AllowOnce)
                .await
                .unwrap();
        });

        // Assert
        assert_eq!(decision, PermissionDecision::AllowOnce);
        let store = fixture.store();
        assert_eq!(store.list_pending(&fixture.session).unwrap().len(), 0);
        assert_eq!(store.list_decisions(&fixture.session).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_queued_request_carries_the_normalized_tool_name() {
        // Arrange
        let fixture = Fixture::new();
        let gate = fixture.gate("/ws", PermissionMode::Prompt, ANSWER_WAIT);
        let request = bash("cargo build");

        // Act
        let (_, queued) = tokio::join!(gate.request(&request), async {
            let first = loop {
                let pending = fixture.store().list_pending(&fixture.session).unwrap();
                if let Some(first) = pending.into_iter().next() {
                    break first;
                }
                tokio::time::sleep(POLL).await;
            };
            fixture
                .answers
                .answer(&fixture.hub, &first.id, PermissionDecision::Deny)
                .await
                .unwrap();
            first
        });

        // Assert
        assert_eq!(queued.tool, "bash");
    }

    #[tokio::test]
    async fn an_unanswered_prompt_is_denied_after_the_timeout_with_no_client() {
        // Arrange
        let fixture = Fixture::new();
        let gate = fixture.gate("/ws", PermissionMode::Prompt, SHORT_TIMEOUT);

        // Act
        let decision = gate.request(&bash("cargo build")).await;

        // Assert
        assert_eq!(decision, PermissionDecision::Deny);
        let store = fixture.store();
        assert_eq!(store.list_pending(&fixture.session).unwrap().len(), 0);
        let recorded = store.list_decisions(&fixture.session).unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(!recorded[0].allowed);
    }

    #[tokio::test]
    async fn an_unanswered_prompt_is_denied_after_the_timeout_with_a_silent_client() {
        // Arrange
        let fixture = Fixture::new();
        let (_connection, mut outbox) = fixture.hub.connect();
        let gate = fixture.gate("/ws", PermissionMode::Prompt, SHORT_TIMEOUT);

        // Act
        let decision = gate.request(&bash("cargo build")).await;

        // Assert
        assert_eq!(decision, PermissionDecision::Deny);
        assert!(outbox.events.try_recv().is_err());
        let recorded = fixture.store().list_decisions(&fixture.session).unwrap();
        assert_eq!(recorded.len(), 1);
        assert!(!recorded[0].allowed);
    }

    #[tokio::test]
    async fn allow_always_stores_an_exact_grant_for_later_requests() {
        // Arrange
        let fixture = Fixture::new();
        let gate = fixture.gate("/ws", PermissionMode::Prompt, ANSWER_WAIT);
        let request = bash("cargo build");

        // Act
        let (first, ()) = tokio::join!(gate.request(&request), async {
            let id = loop {
                let pending = fixture.store().list_pending(&fixture.session).unwrap();
                if let Some(first) = pending.first() {
                    break first.id.clone();
                }
                tokio::time::sleep(POLL).await;
            };
            fixture
                .answers
                .answer(&fixture.hub, &id, PermissionDecision::AllowAlways)
                .await
                .unwrap();
        });
        let again = gate.verdict(&request).await.unwrap();
        let other = gate.verdict(&bash("cargo build --release")).await.unwrap();

        // Assert
        assert_eq!(first, PermissionDecision::AllowAlways);
        assert_eq!(again, Verdict::Allow);
        assert_eq!(other, Verdict::Prompt);
        let key = GrantKey {
            repo_path: "/ws",
            tool: "bash",
            cwd: "/ws/x",
            pattern: "cargo build",
        };
        assert!(fixture.store().lookup_grant(&key).unwrap().is_some());
    }

    #[test]
    fn an_answer_that_arrived_before_the_wait_is_returned_without_deadlock() {
        // Arrange
        let answers = PermissionAnswers::new();
        let id = coda_protocol::types::RequestId::new("r-1");
        answers.slots().insert(
            id.clone(),
            Slot::Answered(PermissionDecision::AllowOnce, Instant::now()),
        );
        let (done, finished) = std::sync::mpsc::channel();

        // Act
        let waiting = Arc::clone(&answers);
        std::thread::spawn(move || {
            let ready = matches!(
                waiting.wait_for(&id),
                Wait::Ready(PermissionDecision::AllowOnce)
            );
            let _ = done.send(ready);
        });

        // Assert
        assert_eq!(finished.recv_timeout(ANSWER_WAIT), Ok(true));
    }

    #[tokio::test]
    async fn an_answer_that_misses_the_handoff_is_recorded_as_the_denial_that_applied() {
        // Arrange
        let fixture = Fixture::new();
        let gate = fixture.gate("/ws", PermissionMode::Prompt, SHORT_TIMEOUT);
        let request = bash("cargo build");

        // Act
        let (decision, ()) = tokio::join!(gate.request(&request), async {
            let id = loop {
                let pending = fixture.store().list_pending(&fixture.session).unwrap();
                if let Some(first) = pending.first() {
                    break first.id.clone();
                }
                tokio::time::sleep(POLL).await;
            };
            // Stored, but never handed to the waiting gate.
            fixture
                .hub
                .resolve_pending(&id, PermissionDecision::AllowOnce)
                .await
                .unwrap();
        });

        // Assert
        assert_eq!(decision, PermissionDecision::Deny);
        let recorded = fixture.store().list_decisions(&fixture.session).unwrap();
        assert!(!recorded.last().unwrap().allowed);
    }

    #[tokio::test]
    async fn a_cancelled_gate_frees_its_slot() {
        // Arrange
        let fixture = Fixture::new();
        let gate = fixture.gate("/ws", PermissionMode::Prompt, ANSWER_WAIT);

        // Act
        let outcome = tokio::time::timeout(SHORT_TIMEOUT, gate.request(&bash("cargo build"))).await;

        // Assert
        assert!(outcome.is_err());
        assert!(fixture.answers.slots().is_empty());
    }

    #[tokio::test]
    async fn answers_nobody_collected_do_not_pile_up() {
        // Arrange
        let fixture = Fixture::new();
        let answers = Arc::new(PermissionAnswers {
            slots: std::sync::Mutex::default(),
            stale_after: Duration::ZERO,
        });
        for command in ["a", "b"] {
            let id = fixture
                .hub
                .register_pending(
                    &fixture.turn,
                    NewPending {
                        session: fixture.session.clone(),
                        tool: "bash".to_owned(),
                        repo_path: "/ws".to_owned(),
                        command: command.to_owned(),
                    },
                )
                .await
                .unwrap();

            // Act
            answers
                .answer(&fixture.hub, &id, PermissionDecision::AllowOnce)
                .await
                .unwrap();
        }

        // Assert
        assert_eq!(answers.slots().len(), 1);
    }
}
