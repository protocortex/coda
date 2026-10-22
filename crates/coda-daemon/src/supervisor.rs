// SPDX-License-Identifier: AGPL-3.0-only

//! Restarts a crashed backend with exponential backoff, up to a cap.
//!
//! The supervisor decides only when to try again. Starting the backend, telling
//! users about a crash and cleaning up the old process group belong to the
//! caller, so the same loop serves every delegated backend.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

/// Restarts allowed after the first crash before the turn is given up.
pub const MAX_RESTARTS: u32 = 3;
/// Longest wait before a restart, however many crashes came before it.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Each further restart waits this many times as long as the one before.
const BACKOFF_FACTOR: u32 = 2;

/// When and how often a crashed backend is restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Wait before the first restart.
    pub base: Duration,
    /// Upper bound of any wait.
    pub max_backoff: Duration,
    /// Restarts allowed per turn.
    pub max_restarts: u32,
}

impl RestartPolicy {
    /// The production policy with `base` as the first wait.
    #[must_use]
    pub const fn new(base: Duration) -> Self {
        Self {
            base,
            max_backoff: MAX_BACKOFF,
            max_restarts: MAX_RESTARTS,
        }
    }

    /// The wait before restart number `restart`, counted from 1.
    #[must_use]
    pub fn backoff(&self, restart: u32) -> Duration {
        let growth = BACKOFF_FACTOR.saturating_pow(restart.saturating_sub(1));
        self.base.saturating_mul(growth).min(self.max_backoff)
    }
}

/// Waits for a duration. Injected so tests record the waits instead of living them.
pub trait Sleeper: fmt::Debug + Send + Sync {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Sleeps on the tokio clock.
#[derive(Debug, Clone, Copy)]
pub struct TokioSleeper;

impl Sleeper for TokioSleeper {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// How one run of the backend ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The run is over for a reason a restart cannot fix: it completed, the
    /// user cancelled, or it could not be started again.
    Finished,
    /// The backend died; the old process group is already gone.
    Crashed,
}

/// How a supervised turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Finished,
    /// The backend kept crashing; `restarts` were tried.
    Exhausted {
        restarts: u32,
    },
}

/// One run of the backend, started again by the [`Supervisor`] after a crash.
pub trait Attempt {
    /// Runs the backend; `restarts` is how many times it was started again so
    /// far, and `may_restart` whether the cap still allows another start.
    fn attempt(&mut self, restarts: u32, may_restart: bool) -> impl Future<Output = Step> + Send;
}

/// Runs a backend again after it crashes.
#[derive(Debug, Clone)]
pub struct Supervisor {
    policy: RestartPolicy,
    sleeper: Arc<dyn Sleeper>,
}

impl Supervisor {
    #[must_use]
    pub fn new(policy: RestartPolicy, sleeper: Arc<dyn Sleeper>) -> Self {
        Self { policy, sleeper }
    }

    /// Runs `attempt`, waiting out the backoff before each restart, until it
    /// finishes or the cap is reached.
    pub async fn run(&self, attempt: &mut impl Attempt) -> Verdict {
        let mut restarts = 0;
        loop {
            let may_restart = restarts < self.policy.max_restarts;
            if attempt.attempt(restarts, may_restart).await == Step::Finished {
                return Verdict::Finished;
            }
            if restarts >= self.policy.max_restarts {
                warn!(restarts, "the backend kept crashing; giving up");
                return Verdict::Exhausted { restarts };
            }
            restarts += 1;
            let wait = self.policy.backoff(restarts);
            info!(restart = restarts, wait_ms = ?wait.as_millis(), step = "backoff", "waiting to restart the backend");
            self.sleeper.sleep(wait).await;
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Mutex;

    use super::*;

    /// Returns at once and remembers how long it was asked to wait.
    #[derive(Default)]
    pub struct RecordingSleeper {
        waits: Mutex<Vec<Duration>>,
        on_sleep: Mutex<Option<Box<dyn Fn() + Send>>>,
    }

    impl std::fmt::Debug for RecordingSleeper {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("RecordingSleeper")
                .field("waits", &self.waits)
                .finish_non_exhaustive()
        }
    }

    impl RecordingSleeper {
        /// Runs `hook` each time a wait is asked for, to act during a backoff.
        pub fn on_sleep(&self, hook: impl Fn() + Send + 'static) {
            *self
                .on_sleep
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(hook));
        }

        pub fn waits(&self) -> Vec<Duration> {
            self.waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl Sleeper for RecordingSleeper {
        fn sleep(&self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
            self.waits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(duration);
            let hook = self
                .on_sleep
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(hook) = hook.as_ref() {
                hook();
            }
            Box::pin(std::future::ready(()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::testing::RecordingSleeper;
    use super::*;

    const BASE: Duration = Duration::from_millis(10);

    fn supervisor(policy: RestartPolicy) -> (Supervisor, Arc<RecordingSleeper>) {
        let sleeper = Arc::new(RecordingSleeper::default());
        (Supervisor::new(policy, sleeper.clone()), sleeper)
    }

    /// Plays the given steps in order, then finishes, and counts the runs.
    struct Scripted {
        steps: Vec<Step>,
        runs: Vec<u32>,
    }

    impl Scripted {
        fn new(steps: &[Step]) -> Self {
            Self {
                steps: steps.iter().rev().copied().collect(),
                runs: Vec::new(),
            }
        }
    }

    impl Attempt for Scripted {
        fn attempt(&mut self, restarts: u32, _: bool) -> impl Future<Output = Step> + Send {
            self.runs.push(restarts);
            std::future::ready(self.steps.pop().unwrap_or(Step::Finished))
        }
    }

    #[tokio::test]
    async fn restarts_wait_with_a_doubling_backoff_that_is_recorded() {
        // Arrange
        let (supervisor, sleeper) = supervisor(RestartPolicy::new(BASE));
        let mut script = Scripted::new(&[Step::Crashed, Step::Crashed, Step::Crashed]);

        // Act
        let verdict = supervisor.run(&mut script).await;

        // Assert
        assert_eq!(verdict, Verdict::Finished);
        assert_eq!(script.runs, [0, 1, 2, 3]);
        assert_eq!(sleeper.waits(), [BASE, BASE * 2, BASE * 4]);
    }

    #[tokio::test]
    async fn the_restart_cap_ends_in_exhaustion_without_a_further_wait() {
        // Arrange
        let (supervisor, sleeper) = supervisor(RestartPolicy::new(BASE));
        let mut script = Scripted::new(&[Step::Crashed; 10]);

        // Act
        let verdict = supervisor.run(&mut script).await;

        // Assert
        assert_eq!(
            verdict,
            Verdict::Exhausted {
                restarts: MAX_RESTARTS
            }
        );
        assert_eq!(script.runs.len(), MAX_RESTARTS as usize + 1);
        assert_eq!(sleeper.waits().len(), MAX_RESTARTS as usize);
    }

    #[tokio::test]
    async fn a_finished_run_is_never_restarted() {
        // Arrange
        let (supervisor, sleeper) = supervisor(RestartPolicy::new(BASE));
        let mut script = Scripted::new(&[Step::Finished]);

        // Act
        let verdict = supervisor.run(&mut script).await;

        // Assert
        assert_eq!(verdict, Verdict::Finished);
        assert_eq!(script.runs, [0]);
        assert_eq!(sleeper.waits(), Vec::<Duration>::new());
    }

    #[test]
    fn the_backoff_never_exceeds_its_maximum() {
        // Arrange
        let policy = RestartPolicy {
            base: BASE,
            max_backoff: BASE * 3,
            max_restarts: 10,
        };

        // Act
        let waits: Vec<_> = (1..=4).map(|n| policy.backoff(n)).collect();

        // Assert
        assert_eq!(waits, [BASE, BASE * 2, BASE * 3, BASE * 3]);
    }
}
