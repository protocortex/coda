// SPDX-License-Identifier: AGPL-3.0-only

//! The permission broker: applies the shared policy with store-backed grants,
//! records every decision, and waits for a client on a prompt verdict.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use coda_config::config::PermissionMode;
use coda_protocol::paths::{classify_target, repo_identity};
use coda_protocol::types::{PermissionDecision, RequestId, SessionId};
use coda_providers::gate::PermissionGate;
use coda_providers::policy::{ClassifiedTarget, PolicyInput, Verdict, decide, grant_pattern};
use coda_providers::tool_request::ToolRequest;
use coda_store::{GrantKey, NewDecision, NewPending, Store, StoreError};
use coda_telemetry::attrs::{CODA_PERMISSION_DECISION, CODA_SESSION_ID, CODA_TOOL_NAME};
use tokio::sync::{broadcast, oneshot};
use tokio::time::timeout;
use tracing::{Instrument, Span, field, info_span};

/// How long a prompt waits for an answer before it is denied.
pub const PERMISSION_TIMEOUT: Duration = Duration::from_mins(5);
/// Name of the span that wraps one permission decision.
pub const DECISION_SPAN: &str = "coda.permission.decision";
/// How long a timed-out prompt waits for an answer that is already being stored.
const ANSWER_HANDOFF: Duration = Duration::from_secs(1);
/// Announcements kept for a slow subscriber before it misses some.
const NOTICE_CAPACITY: usize = 16;
/// Separates the tool from the pattern in a configured grant.
const GRANT_SEPARATOR: char = ':';

/// What the broker needs to know about the session it guards.
#[derive(Debug, Clone)]
pub struct BrokerConfig {
    /// The session decisions are recorded under.
    pub session: SessionId,
    /// Canonical git root, the repo identity of grants; see [`resolve_repo`].
    pub repo: PathBuf,
    /// The session's working directory, where configured grants apply.
    pub cwd: PathBuf,
    /// The root that path tools must stay inside.
    pub workspace_root: PathBuf,
    /// The configured answering policy.
    pub mode: PermissionMode,
    /// How long a prompt waits before it is denied.
    pub prompt_timeout: Duration,
    /// `tool:pattern` entries from `[permissions.grants]`.
    pub grants: Vec<String>,
}

/// Why the broker could not reach a verdict.
#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    /// The store refused a read or a write.
    #[error("the permission store failed: {0}")]
    Store(#[from] StoreError),
    /// The blocking task running a store call did not finish.
    #[error("the permission store task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
    /// A configured grant is not of the form `tool:pattern`.
    #[error("configured grant {entry:?} is not of the form tool:pattern")]
    InvalidGrant {
        /// The entry as written in the config.
        entry: String,
    },
}

/// A permission request that now waits for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNotice {
    /// Identifies the request when answering it.
    pub id: RequestId,
    /// Stable tool name.
    pub tool: String,
    /// What the tool wants to run.
    pub command: String,
}

/// The canonical git root of `cwd`, or `cwd` itself canonicalized when it is
/// not inside a repository, so grants always have a stable scope.
#[must_use]
pub fn resolve_repo(cwd: &Path) -> PathBuf {
    repo_identity(cwd).unwrap_or_else(|| canonical(cwd))
}

type Waiting = tokio::sync::Mutex<HashMap<RequestId, oneshot::Sender<PermissionDecision>>>;

/// Denies a queued request whose prompt is dropped before it is answered.
struct PendingGuard {
    store: Arc<Mutex<Store>>,
    waiting: Arc<Waiting>,
    id: RequestId,
    armed: bool,
}

impl PendingGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Best effort: an answer that landed first leaves the row resolved already.
        let _ = self
            .store
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resolve_pending(&self.id, PermissionDecision::Deny);
        if let Ok(mut waiting) = self.waiting.try_lock() {
            waiting.remove(&self.id);
        }
    }
}

/// Decides the permission requests of one session.
pub struct Broker {
    store: Arc<Mutex<Store>>,
    config: BrokerConfig,
    /// Prompts waiting for an answer, woken by [`Broker::answer`].
    waiting: Arc<Waiting>,
    notices: broadcast::Sender<PendingNotice>,
    expired: broadcast::Sender<PendingNotice>,
}

impl Broker {
    /// Creates the broker, stores the configured grants and denies the prompts
    /// a previous run left unanswered past the timeout.
    ///
    /// # Errors
    /// Returns [`BrokerError::InvalidGrant`] for a malformed configured grant
    /// and [`BrokerError::Store`] when a grant cannot be written.
    pub async fn new(store: Arc<Mutex<Store>>, config: BrokerConfig) -> Result<Self, BrokerError> {
        let seeds = config
            .grants
            .iter()
            .map(|entry| parse_grant(entry))
            .collect::<Result<Vec<_>, _>>()?;
        let broker = Self {
            store,
            config,
            waiting: Arc::default(),
            notices: broadcast::channel(NOTICE_CAPACITY).0,
            expired: broadcast::channel(NOTICE_CAPACITY).0,
        };
        let (repo, cwd) = (broker.repo(), lossy(&canonical(&broker.config.cwd)));
        broker
            .with_store(move |store| {
                for (tool, pattern) in &seeds {
                    store.save_grant(&GrantKey {
                        repo_path: &repo,
                        tool,
                        cwd: &cwd,
                        pattern,
                    })?;
                }
                Ok(())
            })
            .await?;
        broker.expire_stale(now_ms()).await?;
        Ok(broker)
    }

    /// Denies the session's pending requests that have waited at least the
    /// prompt timeout at `now_ms` (Unix milliseconds); returns how many.
    /// Meant for startup, when no prompt of this broker can still be waiting.
    ///
    /// # Errors
    /// Returns [`BrokerError::Store`] when a read or a write fails.
    pub async fn expire_stale(&self, now_ms: i64) -> Result<usize, BrokerError> {
        let session = self.config.session.clone();
        let limit = i64::try_from(self.config.prompt_timeout.as_millis()).unwrap_or(i64::MAX);
        self.with_store(move |store| {
            let mut expired = 0;
            for pending in store.list_pending(&session)? {
                if now_ms.saturating_sub(pending.requested_at) >= limit {
                    store.resolve_pending(&pending.id, PermissionDecision::Deny)?;
                    expired += 1;
                }
            }
            Ok(expired)
        })
        .await
    }

    /// Applies the policy to `request` without waiting for anyone.
    ///
    /// # Errors
    /// Returns [`BrokerError::Store`] when the grant lookup fails.
    pub async fn verdict(&self, request: &ToolRequest) -> Result<Verdict, BrokerError> {
        let workspace_root = &self.config.workspace_root;
        let targets: Vec<ClassifiedTarget> = request
            .target_paths
            .iter()
            .map(|path| ClassifiedTarget {
                path: path.clone(),
                target: classify_target(workspace_root, path),
            })
            .collect();
        let granted = self.is_granted(request).await?;
        Ok(decide(&PolicyInput {
            mode: self.config.mode,
            tool: &request.tool,
            workspace_root,
            targets: &targets,
            granted,
        }))
    }

    /// Records a client's answer and wakes the request waiting on it.
    ///
    /// # Errors
    /// Returns [`BrokerError::Store`] for an unknown or already answered request.
    pub async fn answer(
        &self,
        request: &RequestId,
        decision: PermissionDecision,
    ) -> Result<(), BrokerError> {
        let id = request.clone();
        self.with_store(move |store| store.resolve_pending(&id, decision))
            .await?;
        let waiter = self.waiting.lock().await.remove(request);
        if let Some(tx) = waiter {
            // The prompt may have timed out and gone away; the stored answer stands.
            let _ = tx.send(decision);
        }
        Ok(())
    }

    /// The requests still waiting, for a client that connects or resumes.
    ///
    /// # Errors
    /// Returns [`BrokerError::Store`] when the read fails.
    pub async fn pending(&self) -> Result<Vec<PendingNotice>, BrokerError> {
        let session = self.config.session.clone();
        let listed = self
            .with_store(move |store| store.list_pending(&session))
            .await?;
        Ok(listed
            .into_iter()
            .map(|p| PendingNotice {
                id: p.id,
                tool: p.tool,
                command: p.command,
            })
            .collect())
    }

    /// Announces each request that starts waiting.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<PendingNotice> {
        self.notices.subscribe()
    }

    /// Announces each request denied because no answer came in time.
    #[must_use]
    pub fn subscribe_expired(&self) -> broadcast::Receiver<PendingNotice> {
        self.expired.subscribe()
    }

    async fn decide_request(
        &self,
        request: &ToolRequest,
    ) -> Result<PermissionDecision, BrokerError> {
        let decision = match self.verdict(request).await? {
            Verdict::Allow => PermissionDecision::AllowOnce,
            Verdict::Deny => PermissionDecision::Deny,
            Verdict::Prompt => return self.prompt(request).await,
        };
        self.record_decision(request, decision).await?;
        Ok(decision)
    }

    async fn is_granted(&self, request: &ToolRequest) -> Result<bool, BrokerError> {
        let Some((repo, tool, cwd, pattern)) = self.grant_fields(request) else {
            return Ok(false);
        };
        let found = self
            .with_store(move |store| {
                store.lookup_grant(&GrantKey {
                    repo_path: &repo,
                    tool: &tool,
                    cwd: &cwd,
                    pattern: &pattern,
                })
            })
            .await?;
        Ok(found.is_some())
    }

    /// Repo, tool, cwd and pattern, the exact fields a grant is keyed by.
    ///
    /// `None` when the request's own directory is not inside the session's
    /// repo, so such a request is never granted and never stores a grant.
    fn grant_fields(&self, request: &ToolRequest) -> Option<(String, String, String, String)> {
        let cwd = std::fs::canonicalize(&request.cwd).ok()?;
        if repo_identity(&cwd)? != self.config.repo {
            return None;
        }
        Some((
            self.repo(),
            request.tool.name().to_owned(),
            lossy(&cwd),
            grant_pattern(
                &request.tool,
                request.command.as_deref(),
                &request.target_paths,
            ),
        ))
    }

    fn repo(&self) -> String {
        lossy(&self.config.repo)
    }

    async fn record_decision(
        &self,
        request: &ToolRequest,
        decision: PermissionDecision,
    ) -> Result<(), BrokerError> {
        let new = NewDecision {
            session: self.config.session.clone(),
            repo_path: self.repo(),
            command: describe(request),
            decision,
        };
        self.with_store(move |store| store.record_decision(&new))
            .await
    }

    /// Queues the request, then waits for a client or the timeout; the timeout denies.
    async fn prompt(&self, request: &ToolRequest) -> Result<PermissionDecision, BrokerError> {
        // Resolved before the wait so a retargeted link cannot move the grant.
        let grant = self.grant_fields(request);
        let (id, mut rx) = self.queue(request).await?;
        let mut guard = PendingGuard {
            store: Arc::clone(&self.store),
            waiting: Arc::clone(&self.waiting),
            id: id.clone(),
            armed: true,
        };
        let decision = match timeout(self.config.prompt_timeout, &mut rx).await {
            Ok(answer) => answer.unwrap_or(PermissionDecision::Deny),
            Err(_) => self.deny_unanswered(request, &id, &mut rx).await,
        };
        guard.disarm();
        self.waiting.lock().await.remove(&id);
        if decision != PermissionDecision::AllowAlways {
            return Ok(decision);
        }
        if let Some((repo, tool, cwd, pattern)) = grant {
            self.with_store(move |store| {
                store.save_grant(&GrantKey {
                    repo_path: &repo,
                    tool: &tool,
                    cwd: &cwd,
                    pattern: &pattern,
                })
            })
            .await?;
        }
        Ok(decision)
    }

    /// Stores the pending row and registers its waiter before announcing it, so
    /// an answer can never arrive for a request nobody waits on.
    async fn queue(
        &self,
        request: &ToolRequest,
    ) -> Result<(RequestId, oneshot::Receiver<PermissionDecision>), BrokerError> {
        let new = NewPending {
            session: self.config.session.clone(),
            tool: request.tool.name().to_owned(),
            repo_path: self.repo(),
            command: describe(request),
        };
        let notice_tool = new.tool.clone();
        let command = new.command.clone();
        let mut waiting = self.waiting.lock().await;
        let id = self
            .with_store(move |store| store.add_pending(&new))
            .await?;
        let (tx, rx) = oneshot::channel();
        waiting.insert(id.clone(), tx);
        drop(waiting);
        // No subscriber is fine: the request stays listed for one that resumes.
        let _ = self.notices.send(PendingNotice {
            id: id.clone(),
            tool: notice_tool,
            command,
        });
        Ok((id, rx))
    }

    async fn deny_unanswered(
        &self,
        request: &ToolRequest,
        id: &RequestId,
        rx: &mut oneshot::Receiver<PermissionDecision>,
    ) -> PermissionDecision {
        let pending = id.clone();
        let denied = self
            .with_store(move |store| store.resolve_pending(&pending, PermissionDecision::Deny))
            .await;
        if denied.is_ok() {
            let _ = self.expired.send(PendingNotice {
                id: id.clone(),
                tool: request.tool.name().to_owned(),
                command: describe(request),
            });
            return PermissionDecision::Deny;
        }
        // A client answered first; its answer is on the way.
        if let Ok(Ok(decision)) = timeout(ANSWER_HANDOFF, rx).await {
            return decision;
        }
        // The stored answer never reached us, so the denial is what applied.
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

    /// Runs `work` against the store on the blocking pool.
    async fn with_store<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, BrokerError> {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            // SQLite rolls an interrupted transaction back, so a poisoned guard is safe to reuse.
            work(&mut store.lock().unwrap_or_else(PoisonError::into_inner))
        })
        .await?
        .map_err(BrokerError::from)
    }

    fn decision_span(&self, request: &ToolRequest) -> Span {
        info_span!(
            DECISION_SPAN,
            { CODA_SESSION_ID } = %self.config.session,
            { CODA_TOOL_NAME } = request.tool.name(),
            { CODA_PERMISSION_DECISION } = field::Empty,
        )
    }
}

/// Splits a configured `tool:pattern` grant; the pattern may be empty.
fn parse_grant(entry: &str) -> Result<(String, String), BrokerError> {
    match entry.split_once(GRANT_SEPARATOR) {
        Some((tool, pattern)) if !tool.is_empty() => Ok((tool.to_owned(), pattern.to_owned())),
        _ => Err(BrokerError::InvalidGrant {
            entry: entry.to_owned(),
        }),
    }
}

/// `path` canonicalized, or as given when it cannot be resolved.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn now_ms() -> i64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The text a decision is recorded under: the command, or the tool name.
fn describe(request: &ToolRequest) -> String {
    request
        .command
        .clone()
        .unwrap_or_else(|| request.tool.name().to_owned())
}

fn outcome(decision: PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::AllowOnce => "allow_once",
        PermissionDecision::AllowAlways => "allow_always",
        _ => "deny",
    }
}

impl PermissionGate for Broker {
    async fn request(&self, request: &ToolRequest) -> PermissionDecision {
        let span = self.decision_span(request);
        let decided = self.decide_request(request).instrument(span.clone()).await;
        let decision = decided.unwrap_or_else(|e| {
            tracing::error!(
                session = %self.config.session,
                error = %e,
                "permission broker failed; denying the request"
            );
            PermissionDecision::Deny
        });
        span.record(CODA_PERMISSION_DECISION, outcome(decision));
        decision
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fmt::Debug;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use coda_config::config::PermissionMode;
    use coda_protocol::paths::Target;
    use coda_protocol::types::{BackendKind, Mode, PermissionDecision, SessionId};
    use coda_providers::gate::PermissionGate;
    use coda_providers::policy::{TableRow, Verdict, decision_table};
    use coda_providers::tool_request::{Tool, ToolRequest};
    use coda_store::{GrantKey, NewPending, NewSession, Store};
    use tempfile::TempDir;
    use tracing::Subscriber;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{Layer, Registry};

    use super::{Broker, BrokerConfig, BrokerError, PERMISSION_TIMEOUT, resolve_repo};

    const SHORT_TIMEOUT: Duration = Duration::from_millis(100);
    const ANSWER_WAIT: Duration = Duration::from_secs(10);
    const ESCAPE_LINK: &str = "link";
    const DECISION_SPAN: &str = "coda.permission.decision";
    const DECISION_FIELD: &str = "coda.permission.decision";

    struct Fixture {
        dir: TempDir,
        /// Canonical root holding the `ws` and `other` repositories.
        root: PathBuf,
        store: Arc<Mutex<Store>>,
        session: SessionId,
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
            let root = std::fs::canonicalize(dir.path()).unwrap();
            for repo_dir in ["ws/.git", "ws/x", "ws/y", "other/.git", "bare"] {
                std::fs::create_dir_all(root.join(repo_dir)).unwrap();
            }
            std::os::unix::fs::symlink("/etc", root.join("ws").join(ESCAPE_LINK)).unwrap();
            Self {
                dir,
                root,
                store: Arc::new(Mutex::new(store)),
                session,
            }
        }

        /// The real directory standing for a table path such as `/ws/x`.
        fn real(&self, path: &str) -> PathBuf {
            self.root.join(path.trim_start_matches('/'))
        }

        fn bash(&self, command: &str) -> ToolRequest {
            ToolRequest {
                tool: Tool::Bash,
                target_paths: Vec::new(),
                command: Some(command.to_owned()),
                cwd: self.real("/ws/x"),
            }
        }

        fn edit(&self, path: &str) -> ToolRequest {
            ToolRequest {
                tool: Tool::Edit,
                target_paths: vec![PathBuf::from(path)],
                command: None,
                cwd: self.real("/ws"),
            }
        }

        fn config(&self, repo: &str, mode: PermissionMode) -> BrokerConfig {
            BrokerConfig {
                session: self.session.clone(),
                repo: self.real(repo),
                cwd: self.real("/ws/x"),
                workspace_root: self.real("/ws"),
                mode,
                prompt_timeout: SHORT_TIMEOUT,
                grants: Vec::new(),
            }
        }

        async fn broker(&self, config: BrokerConfig) -> Broker {
            Broker::new(Arc::clone(&self.store), config).await.unwrap()
        }

        fn with_store<T>(&self, work: impl FnOnce(&mut Store) -> T) -> T {
            work(&mut self.store.lock().unwrap_or_else(PoisonError::into_inner))
        }

        fn has_grant(&self, repo: &str, tool: &str, cwd: &str, pattern: &str) -> bool {
            let (repo, cwd) = (self.real(repo), self.real(cwd));
            self.with_store(|store| {
                store
                    .lookup_grant(&GrantKey {
                        repo_path: &repo.to_string_lossy(),
                        tool,
                        cwd: &cwd.to_string_lossy(),
                        pattern,
                    })
                    .unwrap()
                    .is_some()
            })
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

    /// Runs `request` on a broker and answers its prompt with `decision`.
    async fn ask_and_answer(
        broker: &Arc<Broker>,
        request: ToolRequest,
        decision: PermissionDecision,
    ) -> PermissionDecision {
        let mut notices = broker.subscribe();
        let asking = {
            let broker = Arc::clone(broker);
            tokio::spawn(async move { broker.request(&request).await })
        };
        let notice = tokio::time::timeout(ANSWER_WAIT, notices.recv())
            .await
            .unwrap()
            .unwrap();
        broker.answer(&notice.id, decision).await.unwrap();
        tokio::time::timeout(ANSWER_WAIT, asking)
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn the_broker_passes_the_shared_decision_table() {
        // Arrange
        let rows = decision_table();

        for row in rows {
            let fixture = Fixture::new();
            if let Some(stored) = &row.stored {
                let (repo, cwd) = (
                    fixture.real(&stored.repo.to_string_lossy()),
                    fixture.real(&stored.cwd.to_string_lossy()),
                );
                fixture.with_store(|store| {
                    store
                        .save_grant(&GrantKey {
                            repo_path: &repo.to_string_lossy(),
                            tool: &stored.tool,
                            cwd: &cwd.to_string_lossy(),
                            pattern: &stored.pattern,
                        })
                        .unwrap();
                });
            }
            let broker = fixture
                .broker(fixture.config(&row.repo.to_string_lossy(), row.mode))
                .await;

            // Act
            let verdict = broker.verdict(&request_of(&fixture, &row)).await.unwrap();

            // Assert
            assert_eq!(verdict, row.expected, "row: {} ({:?})", row.name, row.mode);
        }
    }

    #[tokio::test]
    async fn an_allowed_and_a_denied_request_are_recorded_and_survive_a_restart() {
        // Arrange
        let fixture = Fixture::new();
        let broker = fixture
            .broker(fixture.config("/ws", PermissionMode::AcceptEdits))
            .await;
        let edit = fixture.edit("src/a.rs");

        // Act
        let allowed = broker.request(&edit).await;
        let denied = broker
            .request(&ToolRequest {
                target_paths: vec![PathBuf::from("/etc/passwd")],
                ..edit
            })
            .await;
        drop(broker);
        let reopened = Store::open(&fixture.dir.path().join("data")).unwrap();
        let recorded = reopened.list_decisions(&fixture.session).unwrap();

        // Assert
        assert_eq!(allowed, PermissionDecision::AllowOnce);
        assert_eq!(denied, PermissionDecision::Deny);
        let outcomes: Vec<bool> = recorded.iter().map(|d| d.allowed).collect();
        assert_eq!(outcomes, [true, false]);
    }

    #[tokio::test]
    async fn an_always_allow_answer_stores_a_grant_for_this_repo_only() {
        // Arrange
        let fixture = Fixture::new();
        let broker = Arc::new(
            fixture
                .broker(fixture.config("/ws", PermissionMode::Prompt))
                .await,
        );
        let mut notices = broker.subscribe();
        let asking = {
            let broker = Arc::clone(&broker);
            let request = fixture.bash("cargo test");
            tokio::spawn(async move { broker.request(&request).await })
        };

        // Act
        let notice = tokio::time::timeout(ANSWER_WAIT, notices.recv())
            .await
            .unwrap()
            .unwrap();
        broker
            .answer(&notice.id, PermissionDecision::AllowAlways)
            .await
            .unwrap();
        let decision = tokio::time::timeout(ANSWER_WAIT, asking)
            .await
            .unwrap()
            .unwrap();

        // Assert
        assert_eq!(decision, PermissionDecision::AllowAlways);
        assert!(fixture.has_grant("/ws", "bash", "/ws/x", "cargo test"));
        assert!(!fixture.has_grant("/other", "bash", "/ws/x", "cargo test"));
        assert_eq!(
            broker.verdict(&fixture.bash("cargo test")).await.unwrap(),
            Verdict::Allow
        );
    }

    #[tokio::test]
    async fn an_unanswered_prompt_is_denied_after_the_timeout_with_no_client() {
        // Arrange
        let fixture = Fixture::new();
        let broker = fixture
            .broker(fixture.config("/ws", PermissionMode::Prompt))
            .await;

        // Act
        let decision = broker.request(&fixture.bash("cargo test")).await;
        let waiting = broker.pending().await.unwrap();
        let recorded = fixture.with_store(|s| s.list_decisions(&fixture.session).unwrap());

        // Assert
        assert_eq!(decision, PermissionDecision::Deny);
        assert_eq!(waiting, Vec::new());
        assert_eq!(recorded.len(), 1);
        assert!(!recorded[0].allowed);
    }

    #[tokio::test]
    async fn a_waiting_request_is_listed_for_a_client_that_resumes() {
        // Arrange
        let fixture = Fixture::new();
        let mut config = fixture.config("/ws", PermissionMode::Prompt);
        config.prompt_timeout = PERMISSION_TIMEOUT;
        let broker = Arc::new(fixture.broker(config).await);
        let mut notices = broker.subscribe();
        let asking = {
            let broker = Arc::clone(&broker);
            let request = fixture.bash("cargo test");
            tokio::spawn(async move { broker.request(&request).await })
        };
        let notice = tokio::time::timeout(ANSWER_WAIT, notices.recv())
            .await
            .unwrap()
            .unwrap();

        // Act
        let listed = broker.pending().await.unwrap();
        broker
            .answer(&notice.id, PermissionDecision::Deny)
            .await
            .unwrap();
        let after = broker.pending().await.unwrap();
        let decision = asking.await.unwrap();

        // Assert
        assert_eq!(listed, [notice]);
        assert_eq!(after, Vec::new());
        assert_eq!(decision, PermissionDecision::Deny);
    }

    #[tokio::test]
    async fn configured_grants_are_stored_for_the_repo_and_cwd() {
        // Arrange
        let fixture = Fixture::new();
        let mut config = fixture.config("/ws", PermissionMode::Prompt);
        config.grants = vec!["bash:cargo test".to_owned(), "edit:".to_owned()];

        // Act
        let broker = fixture.broker(config).await;

        // Assert
        assert!(fixture.has_grant("/ws", "bash", "/ws/x", "cargo test"));
        assert!(fixture.has_grant("/ws", "edit", "/ws/x", ""));
        assert_eq!(
            broker.verdict(&fixture.bash("cargo test")).await.unwrap(),
            Verdict::Allow
        );
    }

    #[tokio::test]
    async fn a_malformed_configured_grant_is_refused() {
        // Arrange
        let fixture = Fixture::new();
        let mut config = fixture.config("/ws", PermissionMode::Prompt);
        config.grants = vec!["bash".to_owned()];

        // Act
        let result = Broker::new(Arc::clone(&fixture.store), config).await;

        // Assert
        assert!(matches!(result, Err(BrokerError::InvalidGrant { .. })));
    }

    #[test]
    fn the_repo_is_the_canonical_git_root_of_the_cwd() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let nested = repo.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        let bare = dir.path().join("bare");
        std::fs::create_dir(&bare).unwrap();

        // Act
        let from_nested = resolve_repo(&nested);
        let from_bare = resolve_repo(&bare);

        // Assert
        assert_eq!(from_nested, std::fs::canonicalize(&repo).unwrap());
        assert_eq!(from_bare, std::fs::canonicalize(&bare).unwrap());
    }

    #[tokio::test]
    async fn an_edit_grant_covers_only_the_exact_file() {
        // Arrange
        let fixture = Fixture::new();
        let broker = Arc::new(
            fixture
                .broker(fixture.config("/ws", PermissionMode::Prompt))
                .await,
        );

        // Act
        let decision = ask_and_answer(
            &broker,
            fixture.edit("src/a.rs"),
            PermissionDecision::AllowAlways,
        )
        .await;
        let same = broker.verdict(&fixture.edit("src/a.rs")).await.unwrap();
        let other = broker.verdict(&fixture.edit("src/b.rs")).await.unwrap();
        let manifest = broker.verdict(&fixture.edit("Cargo.toml")).await.unwrap();

        // Assert
        assert_eq!(decision, PermissionDecision::AllowAlways);
        assert_eq!(same, Verdict::Allow);
        assert_eq!(other, Verdict::Prompt);
        assert_eq!(manifest, Verdict::Prompt);
    }

    #[tokio::test]
    async fn a_grant_does_not_follow_a_retargeted_symlinked_cwd() {
        // Arrange
        let fixture = Fixture::new();
        let link = fixture.real("/cwd-link");
        std::os::unix::fs::symlink(fixture.real("/ws/x"), &link).unwrap();
        let request = ToolRequest {
            cwd: link.clone(),
            ..fixture.bash("cargo test")
        };
        let broker = Arc::new(
            fixture
                .broker(fixture.config("/ws", PermissionMode::Prompt))
                .await,
        );
        ask_and_answer(&broker, request.clone(), PermissionDecision::AllowAlways).await;
        let before = broker.verdict(&request).await.unwrap();

        // Act
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(fixture.real("/other"), &link).unwrap();
        let after = broker.verdict(&request).await.unwrap();

        // Assert
        assert!(fixture.has_grant("/ws", "bash", "/ws/x", "cargo test"));
        assert_eq!(before, Verdict::Allow);
        assert_eq!(after, Verdict::Prompt);
    }

    #[tokio::test]
    async fn a_cwd_outside_the_session_repo_is_never_granted_or_stored() {
        // Arrange
        let fixture = Fixture::new();
        fixture.with_store(|store| {
            store
                .save_grant(&GrantKey {
                    repo_path: &fixture.real("/ws").to_string_lossy(),
                    tool: "bash",
                    cwd: &fixture.real("/bare").to_string_lossy(),
                    pattern: "cargo test",
                })
                .unwrap();
        });
        let broker = Arc::new(
            fixture
                .broker(fixture.config("/ws", PermissionMode::Prompt))
                .await,
        );
        let nested = fixture.real("/ws/inner");
        std::fs::create_dir_all(nested.join(".git")).unwrap();
        let cwds = [fixture.real("/bare"), nested, fixture.real("/missing")];

        for cwd in cwds {
            let request = ToolRequest {
                cwd: cwd.clone(),
                ..fixture.bash("cargo test")
            };

            let asked = ToolRequest {
                command: Some("cargo build".to_owned()),
                ..request.clone()
            };

            // Act
            let before = broker.verdict(&request).await.unwrap();
            ask_and_answer(&broker, asked, PermissionDecision::AllowAlways).await;
            let key = cwd.to_string_lossy().into_owned();
            let stored = [fixture.real("/ws"), cwd.clone()].iter().any(|repo| {
                fixture.with_store(|store| {
                    store
                        .lookup_grant(&GrantKey {
                            repo_path: &repo.to_string_lossy(),
                            tool: "bash",
                            cwd: &key,
                            pattern: "cargo build",
                        })
                        .unwrap()
                        .is_some()
                })
            });

            // Assert
            assert_eq!(before, Verdict::Prompt, "{cwd:?}");
            assert!(!stored, "{cwd:?}");
        }
    }

    #[tokio::test]
    async fn a_dropped_prompt_denies_its_pending_row() {
        // Arrange
        let fixture = Fixture::new();
        let mut config = fixture.config("/ws", PermissionMode::Prompt);
        config.prompt_timeout = PERMISSION_TIMEOUT;
        let broker = Arc::new(fixture.broker(config).await);
        let mut notices = broker.subscribe();
        let asking = {
            let broker = Arc::clone(&broker);
            let request = fixture.bash("cargo test");
            tokio::spawn(async move { broker.request(&request).await })
        };
        tokio::time::timeout(ANSWER_WAIT, notices.recv())
            .await
            .unwrap()
            .unwrap();

        // Act
        asking.abort();
        let _ = asking.await;
        let waiting = broker.pending().await.unwrap();
        let recorded = fixture.with_store(|s| s.list_decisions(&fixture.session).unwrap());

        // Assert
        assert_eq!(waiting, Vec::new());
        assert_eq!(recorded.len(), 1);
        assert!(!recorded[0].allowed);
    }

    #[tokio::test]
    async fn stale_pending_rows_are_denied_and_fresh_ones_are_kept() {
        // Arrange
        let fixture = Fixture::new();
        let mut config = fixture.config("/ws", PermissionMode::Prompt);
        config.prompt_timeout = PERMISSION_TIMEOUT;
        let broker = fixture.broker(config).await;
        fixture.with_store(|store| {
            store
                .add_pending(&NewPending {
                    session: fixture.session.clone(),
                    tool: "bash".to_owned(),
                    repo_path: "/ws".to_owned(),
                    command: "cargo test".to_owned(),
                })
                .unwrap();
        });

        // Act
        let fresh = broker.expire_stale(0).await.unwrap();
        let still_waiting = broker.pending().await.unwrap().len();
        let expired = broker.expire_stale(i64::MAX).await.unwrap();
        let after = broker.pending().await.unwrap().len();

        // Assert
        assert_eq!((fresh, still_waiting), (0, 1));
        assert_eq!((expired, after), (1, 0));
    }

    #[tokio::test]
    async fn a_grant_survives_a_broker_rebuilt_on_the_reopened_store() {
        // Arrange
        let fixture = Fixture::new();
        let broker = Arc::new(
            fixture
                .broker(fixture.config("/ws", PermissionMode::Prompt))
                .await,
        );
        ask_and_answer(
            &broker,
            fixture.bash("cargo test"),
            PermissionDecision::AllowAlways,
        )
        .await;
        drop(broker);
        let reopened = Arc::new(Mutex::new(
            Store::open(&fixture.dir.path().join("data")).unwrap(),
        ));

        // Act
        let rebuilt = Broker::new(reopened, fixture.config("/ws", PermissionMode::Prompt))
            .await
            .unwrap();
        let verdict = rebuilt.verdict(&fixture.bash("cargo test")).await.unwrap();

        // Assert
        assert_eq!(verdict, Verdict::Allow);
    }

    #[tokio::test]
    async fn an_unknown_tool_with_no_client_times_out_to_deny() {
        // Arrange
        let fixture = Fixture::new();
        let broker = fixture
            .broker(fixture.config("/ws", PermissionMode::AcceptEdits))
            .await;
        let request = ToolRequest {
            tool: Tool::Unknown("WebFetch".to_owned()),
            target_paths: Vec::new(),
            command: None,
            cwd: fixture.real("/ws"),
        };

        // Act
        let decision = broker.request(&request).await;

        // Assert
        assert_eq!(decision, PermissionDecision::Deny);
    }

    type SpanRecord = (String, BTreeMap<String, String>);

    #[derive(Clone, Default)]
    struct Spans(Arc<Mutex<Vec<SpanRecord>>>);

    struct Fields<'a>(&'a mut BTreeMap<String, String>);

    impl Visit for Fields<'_> {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl<S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>> Layer<S> for Spans {
        fn on_new_span(&self, attrs: &Attributes<'_>, _: &Id, _: Context<'_, S>) {
            let mut fields = BTreeMap::new();
            attrs.record(&mut Fields(&mut fields));
            self.0
                .lock()
                .unwrap()
                .push((attrs.metadata().name().to_owned(), fields));
        }

        fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
            let name = ctx.span(id).unwrap().name().to_owned();
            let mut spans = self.0.lock().unwrap();
            if let Some((_, fields)) = spans.iter_mut().rev().find(|(n, _)| *n == name) {
                values.record(&mut Fields(fields));
            }
        }
    }

    #[tokio::test]
    async fn each_decision_runs_in_a_span_that_carries_the_outcome() {
        // Arrange
        let spans = Spans::default();
        let _guard = tracing::subscriber::set_default(Registry::default().with(spans.clone()));
        let fixture = Fixture::new();
        let broker = fixture
            .broker(fixture.config("/ws", PermissionMode::DenyAll))
            .await;

        // Act
        let decision = broker.request(&fixture.bash("cargo test")).await;

        // Assert
        assert_eq!(decision, PermissionDecision::Deny);
        let recorded = spans.0.lock().unwrap();
        let (_, fields) = recorded
            .iter()
            .find(|(name, _)| name == DECISION_SPAN)
            .expect("a decision span");
        assert_eq!(fields.get(DECISION_FIELD).map(String::as_str), Some("deny"));
    }
}
