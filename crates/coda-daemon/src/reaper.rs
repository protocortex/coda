// SPDX-License-Identifier: AGPL-3.0-only

//! Startup cleanup after a daemon that died with turns running.

use std::io;
use std::path::Path;
use std::sync::Arc;

use coda_protocol::events::{Event, TIMEOUT_KIND};
use coda_protocol::types::{PermissionDecision, SessionId, TurnId};
use coda_store::{EventRecord, PendingPermission, SessionFilter, StoreError};
use rustix::io::Errno;
use rustix::process::{Pid, Signal, kill_process_group};
use tracing::{info, warn};

use crate::backends::PIDFILE_EXTENSION;
use crate::pidfile::{self, PidRecord, ProcessState};
use crate::subscribe::{Hub, HubError};

/// The pending row keeps the command but not the tool, so the event cannot name it.
const UNKNOWN_TOOL: &str = "unknown";

/// What a startup cleanup did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReapReport {
    /// Backend process groups killed after their identity was verified.
    pub killed: u32,
    /// Pidfiles removed without killing anything.
    pub stale: u32,
    /// Pidfiles left in place because the process could not be checked or killed.
    pub kept: u32,
    /// Permission requests answered as expired.
    pub expired: u32,
}

/// Cleans up what a previous daemon left behind. Run it only while holding
/// the instance lock, so no live daemon owns the children or prompts it finds.
pub async fn reap(pidfile_dir: &Path, hub: &Arc<Hub>) -> ReapReport {
    info!(step = "reap_backends", "looking for orphaned backends");
    let mut report = reap_backends(pidfile_dir, &Live).await;
    info!(
        step = "expire_pending",
        "expiring orphaned permission requests"
    );
    report.expired = expire_pending(hub).await;
    report
}

/// The two system calls the cleanup depends on, injectable for tests.
trait Processes {
    async fn check(&self, record: &PidRecord) -> io::Result<ProcessState>;
    fn kill_group(&self, group: Pid) -> Result<(), Errno>;
}

struct Live;

impl Processes for Live {
    async fn check(&self, record: &PidRecord) -> io::Result<ProcessState> {
        pidfile::check_process(record).await
    }

    fn kill_group(&self, group: Pid) -> Result<(), Errno> {
        kill_process_group(group, Signal::KILL)
    }
}

enum Outcome {
    Killed,
    Stale,
    Kept,
}

async fn reap_backends(dir: &Path, procs: &impl Processes) -> ReapReport {
    let mut report = ReapReport::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "could not list the pidfile directory");
            return report;
        }
    };
    for path in entries.filter_map(|entry| Some(entry.ok()?.path())) {
        if path.extension().is_none_or(|ext| ext != PIDFILE_EXTENSION) {
            continue;
        }
        let outcome = match pidfile::read(&path) {
            Ok(record) => reconcile(procs, &record).await,
            Err(e) => {
                warn!(error = %e, "removing an unreadable pidfile");
                Outcome::Stale
            }
        };
        match outcome {
            Outcome::Killed => report.killed += 1,
            Outcome::Stale => report.stale += 1,
            // The process may still run, so the record is the only handle on it.
            Outcome::Kept => {
                report.kept += 1;
                continue;
            }
        }
        if let Err(e) = std::fs::remove_file(&path) {
            warn!(path = %path.display(), error = %e, "could not remove a pidfile");
        }
    }
    report
}

/// Kills the record's process group, but only when the pid still is the
/// process the record describes, since the pid may have been recycled.
async fn reconcile(procs: &impl Processes, record: &PidRecord) -> Outcome {
    let Some(group) = i32::try_from(record.pid).ok().and_then(Pid::from_raw) else {
        return Outcome::Stale;
    };
    match procs.check(record).await {
        Ok(ProcessState::Gone | ProcessState::Other) => return Outcome::Stale,
        Ok(ProcessState::Same) => {}
        Err(e) => {
            warn!(pid = record.pid, error = %e, "could not check an orphaned backend, keeping its pidfile");
            return Outcome::Kept;
        }
    }
    match procs.kill_group(group) {
        Ok(()) => {
            info!(pid = record.pid, "killed an orphaned backend group");
            Outcome::Killed
        }
        Err(Errno::SRCH) => Outcome::Stale,
        Err(e) => {
            warn!(pid = record.pid, error = %e, "could not kill an orphaned backend group, keeping its pidfile");
            Outcome::Kept
        }
    }
}

/// Answers every open permission request as denied: the daemon that was
/// waiting for the user is gone, so nobody can still be answering it.
async fn expire_pending(hub: &Arc<Hub>) -> u32 {
    let open = hub
        .with_store(|store| {
            let mut open = Vec::new();
            for session in store.list_sessions(&SessionFilter::default())? {
                let pending = store.list_pending(&session.id)?;
                if !pending.is_empty() {
                    open.push((session.id, pending));
                }
            }
            Ok(open)
        })
        .await;
    let open = match open {
        Ok(open) => open,
        Err(e) => {
            warn!(error = %e, "could not list open permission requests");
            return 0;
        }
    };
    let mut expired = 0;
    for (session, pending) in open {
        match expire_session(hub, &session, &pending).await {
            Ok(count) => expired += count,
            Err(e) => warn!(session = %session, error = %e, "could not expire open requests"),
        }
    }
    expired
}

async fn expire_session(
    hub: &Arc<Hub>,
    session: &SessionId,
    pending: &[PendingPermission],
) -> Result<u32, HubError> {
    // An event belongs to a turn, and the turn that raised the request is not
    // recorded. The turn starts only when there is an event to put in it.
    let mut started_turn: Option<TurnId> = None;
    let mut expired = 0;
    for request in pending {
        let turn = if let Some(turn) = &started_turn {
            turn.clone()
        } else {
            let id = session.clone();
            let started = hub
                .with_store_mut(move |store| store.begin_turn(&id))
                .await?;
            started_turn.insert(started).clone()
        };
        let event = Event::Timeout {
            session_id: session.clone(),
            request_id: request.id.clone(),
            tool: UNKNOWN_TOOL.to_owned(),
        };
        let body = serde_json::to_string(&event).map_err(StoreError::from)?;
        let record = EventRecord {
            kind: TIMEOUT_KIND.to_owned(),
            body,
        };
        // Publish before resolving: a failure here leaves the request open for
        // the next start instead of resolved with no timeout event.
        hub.publish(session, &turn, record).await?;
        match hub
            .resolve_pending(&request.id, PermissionDecision::Deny)
            .await
        {
            Ok(()) => expired += 1,
            Err(HubError::Store(StoreError::AlreadyResolved { .. })) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(expired)
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::sync::Arc;
    use std::time::Duration;

    use coda_protocol::types::{BackendKind, Mode, SessionId};
    use coda_store::{NewPending, NewSession, Store};
    use tempfile::TempDir;
    use tokio::process::{Child, Command};

    use rustix::io::Errno;
    use rustix::process::Pid;

    use super::{Processes, ReapReport, reap, reap_backends};
    use crate::pidfile::{self, PidRecord, ProcessState};
    use crate::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};

    const SLEEP: &str = "/bin/sleep";
    const WAIT: Duration = Duration::from_secs(10);
    const EPOCH_START: &str = "Thu Jan  1 00:00:00 1970";

    struct Rig {
        dir: TempDir,
        hub: Arc<Hub>,
        session: SessionId,
    }

    impl Rig {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let mut store = Store::open(&dir.path().join("data")).unwrap();
            let session = store
                .create_session(&NewSession {
                    backend: BackendKind::ClaudeCode,
                    model: None,
                    cwd: "/work".to_owned(),
                    mode: Mode::Default,
                })
                .unwrap();
            let hub = Hub::new(store, DEFAULT_QUEUE_CAPACITY);
            Self { dir, hub, session }
        }

        fn run_dir(&self) -> PathBuf {
            let run = self.dir.path().join("run");
            fs::create_dir_all(&run).unwrap();
            run
        }

        async fn reap(&self) -> ReapReport {
            reap(&self.run_dir(), &self.hub).await
        }

        /// Starts a `sleep` as its own group leader and records it in a pidfile.
        async fn orphan(
            &self,
            name: &str,
            tamper: impl FnOnce(&mut PidRecord),
        ) -> (Child, PathBuf) {
            let child = Command::new(SLEEP)
                .arg("300")
                .process_group(0)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let path = self.run_dir().join(name);
            let mut record = pidfile::write(&path, child.id().unwrap(), Path::new(SLEEP))
                .await
                .unwrap();
            tamper(&mut record);
            fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
            (child, path)
        }
    }

    #[tokio::test]
    async fn a_verified_orphan_group_is_killed_and_its_pidfile_removed() {
        // Arrange
        let rig = Rig::new();
        let (mut child, path) = rig.orphan("1.pid", |_| {}).await;

        // Act
        let report = rig.reap().await;

        // Assert
        let status = tokio::time::timeout(WAIT, child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
        assert_eq!(report.killed, 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_record_whose_pid_was_recycled_is_not_killed() {
        // Arrange
        let rig = Rig::new();
        let (mut child, path) = rig
            .orphan("1.pid", |r| r.started_at = EPOCH_START.to_owned())
            .await;

        // Act
        let report = rig.reap().await;

        // Assert
        assert!(child.try_wait().unwrap().is_none());
        assert_eq!(report.killed, 0);
        assert_eq!(report.stale, 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_record_naming_another_executable_is_not_killed() {
        // Arrange
        let rig = Rig::new();
        let (mut child, path) = rig
            .orphan("1.pid", |r| r.process_name = "claude".to_owned())
            .await;

        // Act
        let report = rig.reap().await;

        // Assert
        assert!(child.try_wait().unwrap().is_none());
        assert_eq!(report.stale, 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_record_with_the_same_name_at_another_path_is_not_killed() {
        // Arrange
        let rig = Rig::new();
        let (mut child, path) = rig
            .orphan("1.pid", |r| {
                r.process_path = Some(PathBuf::from("/tmp/elsewhere/sleep"));
            })
            .await;

        // Act
        let report = rig.reap().await;

        // Assert
        assert!(child.try_wait().unwrap().is_none());
        assert_eq!(report.killed, 0);
        assert_eq!(report.stale, 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_record_with_the_same_path_and_start_time_is_killed() {
        // Arrange
        let rig = Rig::new();
        let (mut child, path) = rig.orphan("1.pid", |_| {}).await;
        let recorded = pidfile::read(&path).unwrap();

        // Act
        let report = rig.reap().await;

        // Assert
        assert!(recorded.process_path.is_some());
        tokio::time::timeout(WAIT, child.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.killed, 1);
    }

    #[tokio::test]
    async fn an_unreadable_pidfile_is_removed_and_other_files_are_left_alone() {
        // Arrange
        let rig = Rig::new();
        let broken = rig.run_dir().join("2.pid");
        let other = rig.run_dir().join("daemon.lock");
        fs::write(&broken, "not json").unwrap();
        fs::write(&other, "").unwrap();

        // Act
        let report = rig.reap().await;

        // Assert
        assert_eq!(report.stale, 1);
        assert!(!broken.exists());
        assert!(other.exists());
    }

    #[tokio::test]
    async fn an_orphaned_pending_permission_is_denied_with_a_timeout_event() {
        // Arrange
        let rig = Rig::new();
        let session = rig.session.clone();
        let pending = NewPending {
            session: session.clone(),
            tool: "bash".to_owned(),
            repo_path: "/work".to_owned(),
            command: "ls".to_owned(),
        };
        rig.hub
            .with_store_mut(move |store| store.add_pending(&pending))
            .await
            .unwrap();

        // Act
        let report = rig.reap().await;

        // Assert
        let id = session.clone();
        let (open, events) = rig
            .hub
            .with_store(move |store| Ok((store.list_pending(&id)?, store.load_events(&id, 0, 10)?)))
            .await
            .unwrap();
        assert_eq!(report.expired, 1);
        assert_eq!(open, Vec::new());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "timeout");
    }

    struct Scripted {
        check: fn() -> io::Result<ProcessState>,
        kill: fn() -> Result<(), Errno>,
    }

    impl Processes for Scripted {
        fn check(&self, _: &PidRecord) -> impl Future<Output = io::Result<ProcessState>> {
            std::future::ready((self.check)())
        }

        fn kill_group(&self, _: Pid) -> Result<(), Errno> {
            (self.kill)()
        }
    }

    #[tokio::test]
    async fn a_pidfile_is_kept_when_the_process_cannot_be_checked() {
        // Arrange
        let rig = Rig::new();
        let (_child, path) = rig.orphan("1.pid", |_| {}).await;
        let procs = Scripted {
            check: || Err(io::Error::other("ps failed")),
            kill: || Ok(()),
        };

        // Act
        let report = reap_backends(&rig.run_dir(), &procs).await;

        // Assert
        assert_eq!(report.kept, 1);
        assert_eq!(report.stale + report.killed, 0);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn a_pidfile_is_kept_when_the_group_cannot_be_killed() {
        // Arrange
        let rig = Rig::new();
        let (_child, path) = rig.orphan("1.pid", |_| {}).await;
        let procs = Scripted {
            check: || Ok(ProcessState::Same),
            kill: || Err(Errno::PERM),
        };

        // Act
        let report = reap_backends(&rig.run_dir(), &procs).await;

        // Assert
        assert_eq!(report.kept, 1);
        assert_eq!(report.killed, 0);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn a_pidfile_of_a_gone_process_is_removed() {
        // Arrange
        let rig = Rig::new();
        let (_child, path) = rig.orphan("1.pid", |_| {}).await;
        let procs = Scripted {
            check: || Ok(ProcessState::Gone),
            kill: || Err(Errno::PERM),
        };

        // Act
        let report = reap_backends(&rig.run_dir(), &procs).await;

        // Assert
        assert_eq!(report.stale, 1);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn expiring_requests_starts_one_turn_and_a_second_start_adds_none() {
        // Arrange
        let rig = Rig::new();
        let session = rig.session.clone();
        for command in ["ls", "pwd"] {
            let pending = NewPending {
                session: session.clone(),
                tool: "bash".to_owned(),
                repo_path: "/work".to_owned(),
                command: command.to_owned(),
            };
            rig.hub
                .with_store_mut(move |store| store.add_pending(&pending))
                .await
                .unwrap();
        }

        // Act
        let first = rig.reap().await;
        let second = rig.reap().await;

        // Assert
        let id = session.clone();
        let next_turn = rig
            .hub
            .with_store_mut(move |store| store.begin_turn(&id))
            .await
            .unwrap();
        assert_eq!((first.expired, second.expired), (2, 0));
        assert_eq!(next_turn.as_str(), "2");
    }
}
