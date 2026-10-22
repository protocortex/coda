// SPDX-License-Identifier: AGPL-3.0-only

//! Aborts a backend that goes silent.
//!
//! Silence is judged by phase. While nobody is answering a permission the
//! clock is paused, because the wait is the user's. After a call was approved
//! the backend runs a tool whose start the stream does not announce, so the
//! limit is a separate long one. The same long limit holds while the backend
//! reports a tool of its own as started and not yet finished. Anything else is
//! idle time.

use std::collections::HashSet;
use std::time::Duration;

use coda_protocol::events::Event;
use tokio::sync::watch;
use tokio::time::sleep;

use crate::gate::PERMISSION_TIMEOUT;

/// How long a backend may stay silent between events.
pub const IDLE_TIMEOUT: Duration = Duration::from_mins(5);
/// How long a backend may stay silent while a tool it runs is in flight.
pub const DELEGATED_TOOL_TIMEOUT: Duration = Duration::from_mins(15);

/// The limits a turn runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub idle: Duration,
    pub delegated_tool: Duration,
    /// How long a permission prompt waits before it is denied.
    pub permission: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            idle: IDLE_TIMEOUT,
            delegated_tool: DELEGATED_TOOL_TIMEOUT,
            permission: PERMISSION_TIMEOUT,
        }
    }
}

impl From<&coda_config::timing::Timing> for Timing {
    fn from(config: &coda_config::timing::Timing) -> Self {
        Self {
            idle: config.idle_timeout,
            delegated_tool: config.delegated_tool,
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

/// What the watchdog was last told: the phase, and the backend tools still running.
#[derive(Debug)]
struct State {
    phase: Phase,
    running_tools: HashSet<String>,
}

impl State {
    /// A pending permission outranks a running tool; a running tool outranks idle.
    fn effective(&self) -> Phase {
        match self.phase {
            Phase::PermissionPending => Phase::PermissionPending,
            Phase::Idle if self.running_tools.is_empty() => Phase::Idle,
            Phase::Idle | Phase::ToolRunning => Phase::ToolRunning,
        }
    }
}

/// Watches one turn for silence.
#[derive(Debug)]
pub struct Watchdog {
    timing: Timing,
    state: watch::Sender<State>,
}

impl Watchdog {
    #[must_use]
    pub fn new(timing: Timing) -> Self {
        Self {
            timing,
            state: watch::Sender::new(State {
                phase: Phase::Idle,
                running_tools: HashSet::new(),
            }),
        }
    }

    /// Moves to `phase` and restarts the clock, even when the phase is the same.
    pub fn set(&self, phase: Phase) {
        self.state.send_modify(|state| state.phase = phase);
    }

    /// Restarts the clock without changing the phase.
    pub fn touch(&self) {
        self.state.send_modify(|_| {});
    }

    /// Back to idle with no tool in flight, for a backend that was started again.
    pub fn reset(&self) {
        self.state.send_modify(|state| {
            state.phase = Phase::Idle;
            state.running_tools.clear();
        });
    }

    /// Notes that the backend started tool `id` on its own and restarts the clock.
    pub fn tool_started(&self, id: &str) {
        self.state.send_modify(|state| {
            state.running_tools.insert(id.to_owned());
        });
    }

    /// Notes that tool `id` finished and restarts the clock.
    pub fn tool_finished(&self, id: &str) {
        self.state.send_modify(|state| {
            state.running_tools.remove(id);
        });
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
        let mut phase = self.state.subscribe();
        loop {
            let current = phase.borrow_and_update().effective();
            let limit = match current {
                Phase::Idle => self.timing.idle,
                Phase::ToolRunning => self.timing.delegated_tool,
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

    use super::{DELEGATED_TOOL_TIMEOUT, IDLE_TIMEOUT, Phase, Timing, Watchdog};

    const SHORT: Duration = Duration::from_millis(10);
    const LONG: Duration = Duration::from_millis(80);
    const QUIET_PERIOD: Duration = Duration::from_millis(250);
    const BOUND: Duration = Duration::from_secs(5);

    fn timing() -> Timing {
        Timing {
            idle: SHORT,
            delegated_tool: LONG,
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
    fn the_delegated_tool_limit_defaults_to_fifteen_minutes_and_exceeds_idle() {
        // Arrange / Act
        let timing = Timing::default();

        // Assert
        assert_eq!(timing.delegated_tool, Duration::from_mins(15));
        assert_eq!(DELEGATED_TOOL_TIMEOUT, timing.delegated_tool);
        assert_eq!(IDLE_TIMEOUT, timing.idle);
        assert!(timing.delegated_tool > timing.idle);
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
    async fn an_approved_call_gets_the_delegated_tool_limit() {
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

    #[tokio::test]
    async fn a_running_backend_tool_gets_the_tool_limit_even_after_other_events() {
        // Arrange
        let watchdog = Watchdog::new(timing());
        watchdog.tool_started("t-1");
        watchdog.observe(&Event::StreamDelta {
            session_id: SessionId::new("s-1"),
            turn_id: TurnId::new("1"),
            text: "x".to_owned(),
        });
        let started = Instant::now();

        // Act
        let phase = timeout(BOUND, watchdog.expired()).await.unwrap();

        // Assert
        assert_eq!(phase, Phase::ToolRunning);
        assert!(started.elapsed() >= LONG);
    }

    #[tokio::test]
    async fn a_finished_backend_tool_returns_to_the_idle_limit() {
        // Arrange
        let watchdog = Watchdog::new(timing());
        watchdog.tool_started("t-1");
        watchdog.tool_finished("t-1");

        // Act
        let phase = timeout(BOUND, watchdog.expired()).await.unwrap();

        // Assert
        assert_eq!(phase, Phase::Idle);
    }

    #[tokio::test]
    async fn a_backend_tool_does_not_unpause_a_pending_permission() {
        // Arrange
        let watchdog = Watchdog::new(timing());
        watchdog.set(Phase::PermissionPending);

        // Act
        watchdog.tool_started("t-1");
        let expired = timeout(QUIET_PERIOD, watchdog.expired()).await;

        // Assert
        assert!(expired.is_err());
    }

    #[tokio::test]
    async fn resetting_forgets_the_tools_in_flight() {
        // Arrange
        let watchdog = Watchdog::new(timing());
        watchdog.tool_started("t-1");

        // Act
        watchdog.reset();
        let phase = timeout(BOUND, watchdog.expired()).await.unwrap();

        // Assert
        assert_eq!(phase, Phase::Idle);
    }
}
