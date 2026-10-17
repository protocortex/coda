// SPDX-License-Identifier: AGPL-3.0-only

//! Aborts a backend that goes silent.
//!
//! Silence is judged by phase. While nobody is answering a permission the
//! clock is paused, because the wait is the user's. After a call was approved
//! the backend runs a tool whose start the stream does not announce, so the
//! limit is a separate long one. Anything else is idle time.

use std::time::Duration;

use coda_protocol::events::Event;
use tokio::sync::watch;
use tokio::time::sleep;

use crate::gate::PERMISSION_TIMEOUT;

/// How long a backend may stay silent between events.
pub const IDLE_TIMEOUT: Duration = Duration::from_mins(5);
/// How long a backend may stay silent after a tool call was approved.
pub const TOOL_EXECUTION_TIMEOUT: Duration = Duration::from_mins(30);

/// The limits a turn runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub idle: Duration,
    pub tool_execution: Duration,
    /// How long a permission prompt waits before it is denied.
    pub permission: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            idle: IDLE_TIMEOUT,
            tool_execution: TOOL_EXECUTION_TIMEOUT,
            permission: PERMISSION_TIMEOUT,
        }
    }
}

impl From<&coda_config::timing::Timing> for Timing {
    fn from(config: &coda_config::timing::Timing) -> Self {
        Self {
            idle: config.idle_timeout,
            tool_execution: config.tool_execution,
            permission: config.permission_default_deny,
        }
    }
}

/// What the backend is doing, as far as the daemon can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    PermissionPending,
    ToolRunning,
}

/// Watches one turn for silence.
#[derive(Debug)]
pub struct Watchdog {
    timing: Timing,
    phase: watch::Sender<Phase>,
}

impl Watchdog {
    #[must_use]
    pub fn new(timing: Timing) -> Self {
        Self {
            timing,
            phase: watch::Sender::new(Phase::Idle),
        }
    }

    /// Moves to `phase` and restarts the clock, even when the phase is the same.
    pub fn set(&self, phase: Phase) {
        self.phase.send_replace(phase);
    }

    /// Restarts the clock according to what `event` says the backend did.
    pub fn observe(&self, event: &Event) {
        self.set(match event {
            Event::Approve { .. } => Phase::ToolRunning,
            _ => Phase::Idle,
        });
    }

    /// Completes with the phase whose limit ran out. Never completes while a
    /// permission is pending.
    pub async fn expired(&self) -> Phase {
        let mut phase = self.phase.subscribe();
        loop {
            let current = *phase.borrow_and_update();
            let limit = match current {
                Phase::Idle => self.timing.idle,
                Phase::ToolRunning => self.timing.tool_execution,
                Phase::PermissionPending => {
                    if phase.changed().await.is_err() {
                        return current;
                    }
                    continue;
                }
            };
            tokio::select! {
                () = sleep(limit) => return current,
                changed = phase.changed() => {
                    if changed.is_err() {
                        return current;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use coda_protocol::events::Event;
    use coda_protocol::types::{RequestId, SessionId, TurnId};
    use tokio::time::timeout;

    use super::{IDLE_TIMEOUT, Phase, TOOL_EXECUTION_TIMEOUT, Timing, Watchdog};

    const SHORT: Duration = Duration::from_millis(10);
    const LONG: Duration = Duration::from_millis(80);
    const QUIET_PERIOD: Duration = Duration::from_millis(250);
    const BOUND: Duration = Duration::from_secs(5);

    fn timing() -> Timing {
        Timing {
            idle: SHORT,
            tool_execution: LONG,
            permission: Duration::from_secs(1),
        }
    }

    fn approve() -> Event {
        Event::Approve {
            session_id: SessionId::new("s-1"),
            request_id: RequestId::new("r-1"),
            tool: "Bash".to_owned(),
        }
    }

    #[test]
    fn the_tool_execution_limit_defaults_to_thirty_minutes_and_exceeds_idle() {
        // Arrange / Act
        let timing = Timing::default();

        // Assert
        assert_eq!(timing.tool_execution, Duration::from_mins(30));
        assert_eq!(TOOL_EXECUTION_TIMEOUT, timing.tool_execution);
        assert_eq!(IDLE_TIMEOUT, timing.idle);
        assert!(timing.tool_execution > timing.idle);
    }

    #[tokio::test]
    async fn a_silent_backend_expires_after_the_idle_limit() {
        // Arrange
        let watchdog = Watchdog::new(timing());

        // Act
        let phase = timeout(BOUND, watchdog.expired()).await;

        // Assert
        assert_eq!(phase.unwrap(), Phase::Idle);
    }

    #[tokio::test]
    async fn the_clock_is_paused_while_a_permission_is_pending() {
        // Arrange
        let watchdog = Watchdog::new(timing());
        watchdog.set(Phase::PermissionPending);

        // Act
        let expired = timeout(QUIET_PERIOD, watchdog.expired()).await;

        // Assert
        assert!(expired.is_err());
    }

    #[tokio::test]
    async fn an_approved_call_gets_the_tool_execution_limit() {
        // Arrange
        let watchdog = Watchdog::new(timing());
        watchdog.observe(&approve());
        let started = Instant::now();

        // Act
        let phase = timeout(BOUND, watchdog.expired()).await.unwrap();

        // Assert
        assert_eq!(phase, Phase::ToolRunning);
        assert!(started.elapsed() >= LONG);
    }

    #[tokio::test]
    async fn an_event_after_the_tool_ran_returns_to_the_idle_limit() {
        // Arrange
        let watchdog = Watchdog::new(timing());
        watchdog.observe(&approve());
        watchdog.observe(&Event::StreamDelta {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("1"),
            text: "x".to_owned(),
        });

        // Act
        let phase = timeout(BOUND, watchdog.expired()).await.unwrap();

        // Assert
        assert_eq!(phase, Phase::Idle);
    }

    #[tokio::test]
    async fn answering_the_permission_restarts_the_clock() {
        // Arrange
        let watchdog = Watchdog::new(timing());
        watchdog.set(Phase::PermissionPending);
        let paused = timeout(QUIET_PERIOD, watchdog.expired()).await;

        // Act
        watchdog.set(Phase::Idle);
        let phase = timeout(BOUND, watchdog.expired()).await;

        // Assert
        assert!(paused.is_err());
        assert_eq!(phase.unwrap(), Phase::Idle);
    }
}
