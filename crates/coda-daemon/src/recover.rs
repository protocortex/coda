// SPDX-License-Identifier: Apache-2.0

//! Startup recovery after a daemon that died with turns running.

use std::path::Path;
use std::sync::Arc;

use coda_protocol::events::{Event, InterruptCause, TIMEOUT_KIND};
use coda_protocol::types::{SessionId, TurnId};
use coda_store::{LogEventKind, NewQueued, Store};
use tracing::{info, warn};

use crate::reaper::{ReapReport, expire_requests, reap};
use crate::subscribe::{Hub, HubError};
use crate::turn::{log_event, store_event};

/// Stored event kind of a turn that a restart cut short.
const INTERRUPTED_KIND: &str = "turn_interrupted";

/// What the agent is told when it resumes a turn the restart cut short.
pub const CONTINUE_PROMPT: &str = "The daemon restarted while you were working, and your last turn was cut short. Before you repeat any action that changes state (a file edit, a command, a commit, a push), check whether it already took effect. Then carry on with the task.";

/// Most continuations one session gets inside [`CONTINUE_WINDOW_MS`], so a
/// crash loop does not keep restarting the same work.
pub const MAX_CONTINUATIONS: u32 = 3;

/// The span, in milliseconds, over which [`MAX_CONTINUATIONS`] counts.
pub const CONTINUE_WINDOW_MS: i64 = 5 * 60 * 1000;

/// What a startup recovery did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Turns that were still running when the previous daemon died.
    pub interrupted: u32,
    /// Sessions that now hold a queued continuation, ready to start.
    pub continued: Vec<SessionId>,
    /// Orphaned backends and permission requests cleaned up.
    pub reaped: ReapReport,
}

/// Marks the turns a dead daemon left running as interrupted, then reaps its
/// orphaned backends and expires its open permission requests. Run it only
/// while holding the instance lock, so no live daemon owns what it finds.
/// A second run finds nothing left to do.
pub async fn recover(pidfile_dir: &Path, hub: &Arc<Hub>) -> RecoveryReport {
    recover_and_continue(pidfile_dir, hub, false).await
}

/// [`recover`], and with `continue_after_restart` it also queues
/// [`CONTINUE_PROMPT`] for each turn the restart cut short. The caller starts
/// the sessions in [`RecoveryReport::continued`] once its turns are ready. A turn
/// that waited on a permission prompt is not continued, since the user never
/// answered it, and a session already holding waiting messages keeps them.
pub async fn recover_and_continue(
    pidfile_dir: &Path,
    hub: &Arc<Hub>,
    continue_after_restart: bool,
) -> RecoveryReport {
    info!(step = "interrupt_turns", "interrupting turns left running");
    let (interrupted, mut continued) = interrupt_turns(hub, continue_after_restart).await;
    if continue_after_restart {
        continued.extend(stranded_continuations(hub, &continued).await);
    }
    let reaped = reap(pidfile_dir, hub).await;
    RecoveryReport {
        interrupted,
        continued,
        reaped,
    }
}

async fn interrupt_turns(hub: &Arc<Hub>, continue_after_restart: bool) -> (u32, Vec<SessionId>) {
    let running = match hub.with_store(Store::unfinished_turns).await {
        Ok(running) => running,
        Err(e) => {
            warn!(error = %e, "could not list the turns left running");
            return (0, Vec::new());
        }
    };
    let mut interrupted = 0;
    let mut continued = Vec::new();
    for (session, turn) in running {
        match interrupt_turn(hub, &session, &turn).await {
            Ok(waited_on_prompt) => {
                interrupted += 1;
                if continue_after_restart
                    && !waited_on_prompt
                    && queue_continuation(hub, &session).await
                {
                    continued.push(session);
                }
            }
            Err(e) => {
                warn!(session = %session, turn = %turn, error = %e, "could not interrupt a turn");
            }
        }
    }
    (interrupted, continued)
}

/// Sessions that still hold a continuation from an earlier start that died
/// before it ran, or could not run, and that are not already in `known`.
async fn stranded_continuations(hub: &Arc<Hub>, known: &[SessionId]) -> Vec<SessionId> {
    let waiting = hub
        .with_store(|store| store.sessions_waiting_with(CONTINUE_PROMPT))
        .await;
    let waiting = match waiting {
        Ok(waiting) => waiting,
        Err(e) => {
            warn!(error = %e, "could not look for continuations left in the queue");
            return Vec::new();
        }
    };
    let mut stranded = Vec::new();
    for session in waiting {
        if known.contains(&session) {
            continue;
        }
        let id = session.clone();
        let used = hub
            .with_store(move |store| {
                store.recent_user_messages(&id, CONTINUE_PROMPT, CONTINUE_WINDOW_MS)
            })
            .await;
        if used.is_ok_and(|used| used < MAX_CONTINUATIONS) {
            stranded.push(session);
        }
    }
    stranded
}

/// Queues the continuation unless the session already used its share of them
/// or holds messages of its own. Returns whether it queued one.
async fn queue_continuation(hub: &Arc<Hub>, session: &SessionId) -> bool {
    let id = session.clone();
    let queued = hub
        .with_store_mut(move |store| {
            let used = store.recent_user_messages(&id, CONTINUE_PROMPT, CONTINUE_WINDOW_MS)?;
            if used >= MAX_CONTINUATIONS || !store.queued_messages(&id)?.is_empty() {
                return Ok(false);
            }
            store.enqueue_message(
                &id,
                NewQueued {
                    text: CONTINUE_PROMPT,
                    max_turns: None,
                    token_cap: None,
                },
            )?;
            Ok(true)
        })
        .await;
    match queued {
        Ok(queued) => queued,
        Err(e) => {
            warn!(session = %session, error = %e, "could not queue a continuation");
            false
        }
    }
}

async fn interrupt_turn(
    hub: &Arc<Hub>,
    session: &SessionId,
    turn: &TurnId,
) -> Result<bool, HubError> {
    let (id, turn_id) = (session.clone(), turn.clone());
    // A start that died between the event and the end leaves the event behind.
    let marked = hub
        .with_store(move |store| store.turn_has_event(&id, &turn_id, INTERRUPTED_KIND))
        .await?;
    if !marked {
        let event = Event::TurnInterrupted {
            session_id: session.clone(),
            turn_id: turn.clone(),
            cause: Some(InterruptCause::DaemonRestarted),
        };
        // Event first: a failure leaves the turn open for the next start
        // instead of ended with no interrupted event.
        store_event(hub, session, turn, &event).await?;
    }
    // The prompts the turn was waiting on can never be answered now.
    let id = session.clone();
    let open = hub.with_store(move |store| store.list_pending(&id)).await?;
    // A start that died after it resolved the prompts finds none open, but the
    // timeout events it stored still show that the turn waited on one.
    let (id, turn_id) = (session.clone(), turn.clone());
    let expired_before = hub
        .with_store(move |store| store.turn_has_event(&id, &turn_id, TIMEOUT_KIND))
        .await?;
    expire_requests(hub, session, turn, &open).await?;
    let (id, turn_id) = (session.clone(), turn.clone());
    hub.with_store_mut(move |store| {
        store.record_log_event(&log_event(LogEventKind::TurnEnded, &id, &turn_id))?;
        store.record_log_event(&log_event(LogEventKind::Error, &id, &turn_id))?;
        store.end_turn(&id, &turn_id)
    })
    .await?;
    Ok(!open.is_empty() || expired_before)
}

#[cfg(all(test, unix))]
mod tests {
    use std::sync::Arc;

    use coda_protocol::types::{BackendKind, Mode, SessionId};
    use coda_store::{EventRecord, NewPending, NewSession, Store};
    use tempfile::TempDir;

    use super::{CONTINUE_PROMPT, MAX_CONTINUATIONS, recover, recover_and_continue};
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
    async fn a_recovered_turn_is_interrupted_with_the_daemon_restarted_cause() {
        // Arrange
        let rig = Rig::new();
        let id = rig.session.clone();
        rig.hub
            .with_store_mut(move |store| store.begin_turn(&id))
            .await
            .unwrap();

        // Act
        recover(&rig.dir.path().join("run"), &rig.hub).await;

        // Assert
        let id = rig.session.clone();
        let events = rig
            .hub
            .with_store(move |store| store.load_events(&id, 0, 10))
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&events[0].body).unwrap();
        assert_eq!(body["cause"], "daemon_restarted");
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
            agent: None,
            reason: None,
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

    async fn queued(rig: &Rig) -> Vec<String> {
        let id = rig.session.clone();
        rig.hub
            .with_store(move |store| store.queued_messages(&id))
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.text)
            .collect()
    }

    async fn begin(rig: &Rig) {
        let id = rig.session.clone();
        rig.hub
            .with_store_mut(move |store| store.begin_turn(&id))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_cut_short_turn_gets_a_queued_continuation_when_asked() {
        // Arrange
        let rig = Rig::new();
        begin(&rig).await;

        // Act
        let report = recover_and_continue(&rig.dir.path().join("run"), &rig.hub, true).await;

        // Assert
        assert_eq!(report.continued, std::slice::from_ref(&rig.session));
        assert_eq!(queued(&rig).await, [CONTINUE_PROMPT]);
    }

    #[tokio::test]
    async fn nothing_is_queued_unless_asked() {
        // Arrange
        let rig = Rig::new();
        begin(&rig).await;

        // Act
        let report = recover_and_continue(&rig.dir.path().join("run"), &rig.hub, false).await;

        // Assert
        assert_eq!(report.continued, Vec::<SessionId>::new());
        assert_eq!(queued(&rig).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_turn_that_waited_on_a_prompt_is_not_continued() {
        // Arrange
        let rig = Rig::new();
        let id = rig.session.clone();
        let pending = NewPending {
            session: id.clone(),
            tool: "bash".to_owned(),
            repo_path: "/work".to_owned(),
            command: "ls".to_owned(),
            agent: None,
            reason: None,
        };
        rig.hub
            .with_store_mut(move |store| {
                store.begin_turn(&id)?;
                store.add_pending(&pending)
            })
            .await
            .unwrap();

        // Act
        let report = recover_and_continue(&rig.dir.path().join("run"), &rig.hub, true).await;

        // Assert
        assert_eq!(report.interrupted, 1);
        assert_eq!(report.continued, Vec::<SessionId>::new());
        assert_eq!(queued(&rig).await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_session_past_its_continuations_gets_no_more() {
        // Arrange
        let rig = Rig::new();
        let id = rig.session.clone();
        rig.hub
            .with_store_mut(move |store| {
                for _ in 0..MAX_CONTINUATIONS {
                    let turn = store.begin_turn(&id)?;
                    let record = EventRecord {
                        kind: "user_message".to_owned(),
                        body: serde_json::json!({ "text": CONTINUE_PROMPT }).to_string(),
                    };
                    store.append_event(&id, &turn, &record)?;
                    store.end_turn(&id, &turn)?;
                }
                store.begin_turn(&id)
            })
            .await
            .unwrap();

        // Act
        let report = recover_and_continue(&rig.dir.path().join("run"), &rig.hub, true).await;

        // Assert
        assert_eq!(report.interrupted, 1);
        assert_eq!(report.continued, Vec::<SessionId>::new());
    }

    #[tokio::test]
    async fn waiting_messages_keep_their_place_over_a_continuation() {
        // Arrange
        let rig = Rig::new();
        begin(&rig).await;
        let id = rig.session.clone();
        rig.hub
            .with_store_mut(move |store| {
                store.enqueue_message(
                    &id,
                    coda_store::NewQueued {
                        text: "mine",
                        max_turns: None,
                        token_cap: None,
                    },
                )
            })
            .await
            .unwrap();

        // Act
        let report = recover_and_continue(&rig.dir.path().join("run"), &rig.hub, true).await;

        // Assert
        assert_eq!(report.continued, Vec::<SessionId>::new());
        assert_eq!(queued(&rig).await, ["mine"]);
    }

    #[tokio::test]
    async fn a_continuation_left_in_the_queue_is_started_again() {
        // Arrange
        let rig = Rig::new();
        let id = rig.session.clone();
        rig.hub
            .with_store_mut(move |store| {
                store.enqueue_message(
                    &id,
                    coda_store::NewQueued {
                        text: CONTINUE_PROMPT,
                        max_turns: None,
                        token_cap: None,
                    },
                )
            })
            .await
            .unwrap();

        // Act
        let report = recover_and_continue(&rig.dir.path().join("run"), &rig.hub, true).await;

        // Assert
        assert_eq!(report.continued, std::slice::from_ref(&rig.session));
        assert_eq!(queued(&rig).await, [CONTINUE_PROMPT]);
    }

    #[tokio::test]
    async fn a_turn_whose_prompts_expired_in_an_earlier_start_is_not_continued() {
        // Arrange
        let rig = Rig::new();
        let id = rig.session.clone();
        rig.hub
            .with_store_mut(move |store| {
                let turn = store.begin_turn(&id)?;
                let record = EventRecord {
                    kind: coda_protocol::events::TIMEOUT_KIND.to_owned(),
                    body: "{}".to_owned(),
                };
                store.append_event(&id, &turn, &record)
            })
            .await
            .unwrap();

        // Act
        let report = recover_and_continue(&rig.dir.path().join("run"), &rig.hub, true).await;

        // Assert
        assert_eq!(report.interrupted, 1);
        assert_eq!(report.continued, Vec::<SessionId>::new());
    }
}
