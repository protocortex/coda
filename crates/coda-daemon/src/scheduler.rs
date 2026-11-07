// SPDX-License-Identifier: Apache-2.0

//! Scheduled prompts: every [`TICK`] the daemon claims the prompts that are due and
//! sends each into its session, or into a new session made for the run.
//!
//! A prompt is claimed in the store before it is sent, so a tick never sends one twice
//! and a crash leaves a row that [`recover`] puts back. Each tick sends at most
//! [`MAX_RUNS_PER_TICK`] prompts, which with the one-minute floor on intervals and the
//! cap on how many schedules exist bounds how often the daemon can be made to start turns.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use coda_protocol::methods::{ScheduleInfo, ScheduleTarget, SendMode, SessionSendParams};
use coda_protocol::types::SessionId;
use coda_store::NewSession;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::subscribe::{Hub, HubError};
use crate::turn::Turns;

/// How often the daemon looks for prompts that are due.
pub const TICK: Duration = Duration::from_secs(15);
/// Most prompts one tick sends.
pub const MAX_RUNS_PER_TICK: usize = 5;

/// Sends the prompt of one run.
pub(crate) trait RunTarget {
    /// Sends the prompt of `schedule` to where it goes, or says why it could not.
    fn run(&self, schedule: &ScheduleInfo) -> impl Future<Output = Result<(), String>> + Send;
}

/// Sends a prompt through the daemon's turns.
pub(crate) struct TurnsRunner {
    hub: Arc<Hub>,
    turns: Arc<Turns>,
}

impl TurnsRunner {
    pub(crate) fn new(hub: Arc<Hub>, turns: Arc<Turns>) -> Self {
        Self { hub, turns }
    }

    async fn session_for(&self, target: &ScheduleTarget) -> Result<SessionId, String> {
        match target {
            ScheduleTarget::Session { session_id } => Ok(session_id.clone()),
            ScheduleTarget::NewSession {
                backend,
                model,
                cwd,
                mode,
            } => {
                let cwd = crate::session_rpc::canonical_cwd(cwd).await.map_err(|e| {
                    format!("the working directory {cwd} cannot be used: {}", e.message)
                })?;
                let new = NewSession {
                    backend: *backend,
                    model: model.clone(),
                    cwd,
                    mode: *mode,
                };
                self.hub
                    .with_store_mut(move |store| store.create_session(&new))
                    .await
                    .map_err(|e: HubError| e.to_string())
            }
        }
    }
}

impl RunTarget for TurnsRunner {
    async fn run(&self, schedule: &ScheduleInfo) -> Result<(), String> {
        let session_id = self.session_for(&schedule.target).await?;
        let send = SessionSendParams {
            session_id,
            text: schedule.text.clone(),
            max_turns: None,
            token_cap: None,
            mode: SendMode::Queue,
            client_request_id: None,
        };
        self.turns
            .send(send)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Puts back the prompts a crash left running. Run it once, before the first tick.
pub async fn recover(hub: &Arc<Hub>) {
    match hub
        .with_store_mut(coda_store::Store::recover_schedules)
        .await
    {
        Ok(0) => {}
        Ok(found) => info!(
            found,
            "scheduled prompts left running by a crash were put back"
        ),
        Err(e) => warn!(error = %e, "could not recover the scheduled prompts"),
    }
}

/// Claims what is due and sends it. Returns how many prompts it sent or tried to send.
pub(crate) async fn tick<R: RunTarget>(hub: &Arc<Hub>, runner: &R) -> usize {
    let claimed = match hub
        .with_store_mut(|store| store.claim_due_schedules(MAX_RUNS_PER_TICK))
        .await
    {
        Ok(claimed) => claimed,
        Err(e) => {
            warn!(error = %e, "could not look for scheduled prompts that are due");
            return 0;
        }
    };
    for schedule in &claimed {
        match runner.run(schedule).await {
            Ok(()) => info!(
                schedule = schedule.id,
                run = schedule.runs,
                "sent a scheduled prompt"
            ),
            Err(reason) => {
                warn!(schedule = schedule.id, %reason, "could not send a scheduled prompt");
            }
        }
        let id = schedule.id;
        if let Err(e) = hub
            .with_store_mut(move |store| store.finish_schedule_run(id))
            .await
        {
            warn!(schedule = id, error = %e, "could not end the run of a scheduled prompt");
        }
    }
    claimed.len()
}

/// Recovers, then ticks until the daemon stops.
pub fn spawn(hub: Arc<Hub>, turns: Arc<Turns>) -> JoinHandle<()> {
    tokio::spawn(async move {
        recover(&hub).await;
        let runner = TurnsRunner::new(Arc::clone(&hub), turns);
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            tick(&hub, &runner).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, Ordering};

    use coda_protocol::methods::{ScheduleSpec, ScheduleState};
    use coda_protocol::types::{BackendKind, Mode};
    use coda_store::{Clock, Store};

    use super::*;
    use crate::subscribe::DEFAULT_QUEUE_CAPACITY;

    const START: i64 = 1_700_000_000_000;
    const MINUTE_MS: i64 = 60_000;

    #[derive(Debug)]
    struct Moving(AtomicI64);

    impl Clock for Moving {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct Recording {
        sent: Mutex<Vec<(u64, String)>>,
        fail: bool,
    }

    impl RunTarget for Recording {
        fn run(&self, schedule: &ScheduleInfo) -> impl Future<Output = Result<(), String>> + Send {
            self.sent
                .lock()
                .unwrap()
                .push((schedule.id, schedule.text.clone()));
            let result = if self.fail {
                Err("down".to_owned())
            } else {
                Ok(())
            };
            std::future::ready(result)
        }
    }

    struct Rig {
        _dir: tempfile::TempDir,
        hub: Arc<Hub>,
        clock: Arc<Moving>,
        session: SessionId,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(Moving(AtomicI64::new(START)));
        let mut store =
            Store::open_with_clock(&dir.path().join("data"), Arc::clone(&clock) as _).unwrap();
        let session = store
            .create_session(&NewSession {
                backend: BackendKind::Faux,
                model: None,
                cwd: "/repo".to_owned(),
                mode: Mode::Default,
            })
            .unwrap();
        Rig {
            hub: Hub::new(store, DEFAULT_QUEUE_CAPACITY),
            _dir: dir,
            clock,
            session,
        }
    }

    async fn add(rig: &Rig, spec: ScheduleSpec, text: &str) -> u64 {
        let target = ScheduleTarget::Session {
            session_id: rig.session.clone(),
        };
        let text = text.to_owned();
        rig.hub
            .with_store_mut(move |store| store.add_schedule(&spec, &target, &text))
            .await
            .unwrap()
            .id
    }

    fn recording(fail: bool) -> Recording {
        Recording {
            sent: Mutex::default(),
            fail,
        }
    }

    async fn states(rig: &Rig) -> Vec<ScheduleState> {
        rig.hub
            .with_store(coda_store::Store::list_schedules)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.state)
            .collect()
    }

    #[tokio::test]
    async fn a_due_prompt_is_sent_once_and_then_waits_for_its_next_run() {
        // Arrange
        let rig = rig();
        add(
            &rig,
            ScheduleSpec::Interval { every_secs: 60 },
            "check the build",
        )
        .await;
        let runner = recording(false);
        rig.clock.0.fetch_add(MINUTE_MS, Ordering::SeqCst);

        // Act
        let first = tick(&rig.hub, &runner).await;
        let second = tick(&rig.hub, &runner).await;

        // Assert
        assert_eq!((first, second), (1, 0));
        assert_eq!(runner.sent.lock().unwrap().len(), 1);
        assert_eq!(states(&rig).await, [ScheduleState::Idle]);
    }

    #[tokio::test]
    async fn a_prompt_that_is_not_due_is_not_sent() {
        // Arrange
        let rig = rig();
        add(&rig, ScheduleSpec::Interval { every_secs: 60 }, "later").await;
        let runner = recording(false);

        // Act
        let sent = tick(&rig.hub, &runner).await;

        // Assert
        assert_eq!(sent, 0);
        assert_eq!(runner.sent.lock().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn a_run_that_fails_still_ends_so_the_prompt_is_not_stuck_running() {
        // Arrange
        let rig = rig();
        add(&rig, ScheduleSpec::Interval { every_secs: 60 }, "hi").await;
        let runner = recording(true);
        rig.clock.0.fetch_add(MINUTE_MS, Ordering::SeqCst);

        // Act
        tick(&rig.hub, &runner).await;

        // Assert
        assert_eq!(states(&rig).await, [ScheduleState::Idle]);
    }

    #[tokio::test]
    async fn a_one_time_prompt_is_done_after_its_run() {
        // Arrange
        let rig = rig();
        add(
            &rig,
            ScheduleSpec::Once {
                at: START + MINUTE_MS,
            },
            "once",
        )
        .await;
        let runner = recording(false);
        rig.clock.0.fetch_add(MINUTE_MS, Ordering::SeqCst);

        // Act
        tick(&rig.hub, &runner).await;
        rig.clock.0.fetch_add(10 * MINUTE_MS, Ordering::SeqCst);
        let again = tick(&rig.hub, &runner).await;

        // Assert
        assert_eq!(again, 0);
        assert_eq!(states(&rig).await, [ScheduleState::Done]);
    }

    #[tokio::test]
    async fn one_tick_sends_at_most_the_cap_and_the_rest_go_on_the_next() {
        // Arrange
        let rig = rig();
        for n in 0..=MAX_RUNS_PER_TICK {
            add(
                &rig,
                ScheduleSpec::Interval { every_secs: 60 },
                &format!("p{n}"),
            )
            .await;
        }
        let runner = recording(false);
        rig.clock.0.fetch_add(MINUTE_MS, Ordering::SeqCst);

        // Act
        let first = tick(&rig.hub, &runner).await;
        let second = tick(&rig.hub, &runner).await;

        // Assert
        assert_eq!(first, MAX_RUNS_PER_TICK);
        assert_eq!(second, 1);
    }

    #[tokio::test]
    async fn recovery_frees_a_prompt_a_crash_left_running() {
        // Arrange
        let rig = rig();
        add(&rig, ScheduleSpec::Interval { every_secs: 60 }, "hi").await;
        rig.clock.0.fetch_add(MINUTE_MS, Ordering::SeqCst);
        rig.hub
            .with_store_mut(|store| store.claim_due_schedules(10))
            .await
            .unwrap();

        // Act
        recover(&rig.hub).await;

        // Assert
        assert_eq!(states(&rig).await, [ScheduleState::Idle]);
    }
}
