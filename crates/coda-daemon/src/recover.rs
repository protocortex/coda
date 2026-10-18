// SPDX-License-Identifier: AGPL-3.0-only

//! Startup recovery after a daemon that died with turns running.

use std::path::Path;
use std::sync::Arc;

use coda_protocol::events::Event;
use coda_protocol::types::{SessionId, TurnId};
use coda_store::{LogEventKind, Store};
use tracing::{info, warn};

use crate::reaper::{ReapReport, expire_requests, reap};
use crate::subscribe::{Hub, HubError};
use crate::turn::{log_event, store_event};

/// Stored event kind of a turn that a restart cut short.
const INTERRUPTED_KIND: &str = "turn_interrupted";

/// What a startup recovery did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Turns that were still running when the previous daemon died.
    pub interrupted: u32,
    /// Orphaned backends and permission requests cleaned up.
    pub reaped: ReapReport,
}

/// Marks the turns a dead daemon left running as interrupted, then reaps its
/// orphaned backends and expires its open permission requests. Run it only
/// while holding the instance lock, so no live daemon owns what it finds.
/// A second run finds nothing left to do.
pub async fn recover(pidfile_dir: &Path, hub: &Arc<Hub>) -> RecoveryReport {
    info!(step = "interrupt_turns", "interrupting turns left running");
    let interrupted = interrupt_turns(hub).await;
    let reaped = reap(pidfile_dir, hub).await;
    RecoveryReport {
        interrupted,
        reaped,
    }
}

async fn interrupt_turns(hub: &Arc<Hub>) -> u32 {
    let running = match hub.with_store(Store::unfinished_turns).await {
        Ok(running) => running,
        Err(e) => {
            warn!(error = %e, "could not list the turns left running");
            return 0;
        }
    };
    let mut interrupted = 0;
    for (session, turn) in running {
        match interrupt_turn(hub, &session, &turn).await {
            Ok(()) => interrupted += 1,
            Err(e) => {
                warn!(session = %session, turn = %turn, error = %e, "could not interrupt a turn");
            }
        }
    }
    interrupted
}

async fn interrupt_turn(
    hub: &Arc<Hub>,
    session: &SessionId,
    turn: &TurnId,
) -> Result<(), HubError> {
    let (id, turn_id) = (session.clone(), turn.clone());
    // A start that died between the event and the end leaves the event behind.
    let marked = hub
        .with_store(move |store| store.turn_has_event(&id, &turn_id, INTERRUPTED_KIND))
        .await?;
    if !marked {
        let event = Event::TurnInterrupted {
            session_id: session.clone(),
            turn_id: turn.clone(),
        };
        // Event first: a failure leaves the turn open for the next start
        // instead of ended with no interrupted event.
        store_event(hub, session, turn, &event).await?;
    }
    // The prompts the turn was waiting on can never be answered now.
    let id = session.clone();
    let open = hub.with_store(move |store| store.list_pending(&id)).await?;
    expire_requests(hub, session, turn, &open).await?;
    let (id, turn_id) = (session.clone(), turn.clone());
    hub.with_store_mut(move |store| {
        store.record_log_event(&log_event(LogEventKind::TurnEnded, &id, &turn_id))?;
        store.record_log_event(&log_event(LogEventKind::Error, &id, &turn_id))?;
        store.end_turn(&id, &turn_id)
    })
    .await
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;

    use coda_protocol::types::{BackendKind, Mode, SessionId};
    use coda_store::{EventRecord, NewPending, NewSession, Store};
    use tempfile::TempDir;

    use super::recover;
    use crate::subscribe::{DEFAULT_QUEUE_CAPACITY, Hub};

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

        async fn kinds(&self) -> Vec<String> {
            let id = self.session.clone();
            let events = self
                .hub
                .with_store(move |store| store.load_events(&id, 0, 10))
                .await
                .unwrap();
            events.into_iter().map(|e| e.kind).collect()
        }
    }

    #[tokio::test]
    async fn a_turn_left_running_is_interrupted_once() {
        // Arrange
        let rig = Rig::new();
        let id = rig.session.clone();
        rig.hub
            .with_store_mut(move |store| store.begin_turn(&id))
            .await
            .unwrap();
        let run_dir = rig.dir.path().join("run");

        // Act
        let first = recover(&run_dir, &rig.hub).await;
        let second = recover(&run_dir, &rig.hub).await;

        // Assert
        assert_eq!((first.interrupted, second.interrupted), (1, 0));
        assert_eq!(rig.kinds().await, ["turn_interrupted"]);
    }

    #[tokio::test]
    async fn a_finished_turn_is_left_alone() {
        // Arrange
        let rig = Rig::new();
        let id = rig.session.clone();
        rig.hub
            .with_store_mut(move |store| {
                let turn = store.begin_turn(&id)?;
                store.end_turn(&id, &turn)
            })
            .await
            .unwrap();

        // Act
        let report = recover(&rig.dir.path().join("run"), &rig.hub).await;

        // Assert
        assert_eq!(report.interrupted, 0);
        assert_eq!(rig.kinds().await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_turn_already_marked_interrupted_gets_no_second_event() {
        // Arrange
        let rig = Rig::new();
        let id = rig.session.clone();
        rig.hub
            .with_store_mut(move |store| {
                let turn = store.begin_turn(&id)?;
                let marker = EventRecord {
                    kind: "turn_interrupted".to_owned(),
                    body: "{}".to_owned(),
                };
                store.append_event(&id, &turn, &marker).map(drop)
            })
            .await
            .unwrap();

        // Act
        let report = recover(&rig.dir.path().join("run"), &rig.hub).await;
        let again = recover(&rig.dir.path().join("run"), &rig.hub).await;

        // Assert
        assert_eq!((report.interrupted, again.interrupted), (1, 0));
        assert_eq!(rig.kinds().await, ["turn_interrupted"]);
    }

    #[tokio::test]
    async fn open_prompts_expire_inside_the_interrupted_turn_without_a_new_turn() {
        // Arrange
        let rig = Rig::new();
        let id = rig.session.clone();
        let pending = NewPending {
            session: id.clone(),
            tool: "bash".to_owned(),
            repo_path: "/work".to_owned(),
            command: "ls".to_owned(),
        };
        let turn = rig
            .hub
            .with_store_mut(move |store| {
                let turn = store.begin_turn(&id)?;
                store.add_pending(&pending)?;
                Ok(turn)
            })
            .await
            .unwrap();

        // Act
        let report = recover(&rig.dir.path().join("run"), &rig.hub).await;

        // Assert
        let id = rig.session.clone();
        let (events, next, open) = rig
            .hub
            .with_store_mut(move |store| {
                Ok((
                    store.load_events(&id, 0, 10)?,
                    store.begin_turn(&id)?,
                    store.list_pending(&id)?,
                ))
            })
            .await
            .unwrap();
        let placed: Vec<_> = events.iter().map(|e| (e.kind.as_str(), &e.turn)).collect();
        assert_eq!(placed, [("turn_interrupted", &turn), ("timeout", &turn)]);
        assert_eq!(report.interrupted, 1);
        assert_eq!((next.as_str(), open.len()), ("2", 0));
    }
}
