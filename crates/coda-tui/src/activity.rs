// SPDX-License-Identifier: Apache-2.0

//! The words for the state of a running turn: working, stopping, not responding.
//!
//! No terminal code and no clock of its own, so the rules are tested without waiting.

use std::time::{Duration, Instant};

use crate::work::format_duration;

/// How long a stop may take before the UI offers to force it.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(5);
const STOPPING: &str = "Stopping";
const NOT_RESPONDING: &str = "not responding, press Ctrl+C again to force stop";
const NOT_RESPONDING_NO_FORCE: &str = "not responding";
const FORCING: &str = "Forcing stop";
const WORKING: &str = "Working";

/// How far a stop that the user asked for has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Asked for, and still inside [`STOP_TIMEOUT`].
    Stopping,
    /// Asked for, and the turn did not end in time. Another Ctrl+C forces it.
    NotResponding,
    /// The user forced it a moment ago; the daemon is ending the turn.
    Forcing,
}

/// The stage of a stop asked for at `asked`, seen at `now`. A force counts
/// from the moment it was sent, and one that did not end the turn in time is
/// offered again.
#[must_use]
pub fn stage(asked: Instant, forced: bool, now: Instant) -> Stage {
    if now.saturating_duration_since(asked) >= STOP_TIMEOUT {
        Stage::NotResponding
    } else if forced {
        Stage::Forcing
    } else {
        Stage::Stopping
    }
}

/// What the status line says about a turn that runs. `stop` is when the user
/// asked to stop it, or last forced it, and whether that was a force.
/// `can_force` is whether the daemon honors a force.
#[must_use]
pub fn status_text(
    began: Instant,
    stop: Option<(Instant, bool)>,
    can_force: bool,
    now: Instant,
) -> String {
    if let Some((asked, forced)) = stop {
        return match stage(asked, forced, now) {
            Stage::Stopping => STOPPING,
            Stage::NotResponding if can_force => NOT_RESPONDING,
            Stage::NotResponding => NOT_RESPONDING_NO_FORCE,
            Stage::Forcing => FORCING,
        }
        .to_owned();
    }
    let time = format_duration(now.saturating_duration_since(began));
    if time.is_empty() {
        WORKING.to_owned()
    } else {
        format!("{WORKING} {time}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_running_turn_reads_working_with_its_time() {
        let t0 = Instant::now();

        assert_eq!(status_text(t0, None, true, t0), "Working");
        assert_eq!(status_text(t0, None, true, t0 + secs(9)), "Working 9s");
        assert_eq!(status_text(t0, None, true, t0 + secs(64)), "Working 1m 04s");
    }

    #[test]
    fn a_stop_reads_stopping_until_the_timeout() {
        let t0 = Instant::now();
        let asked = t0 + secs(10);

        let text = status_text(t0, Some((asked, false)), true, asked + secs(1));

        assert_eq!(text, "Stopping");
    }

    #[test]
    fn a_stop_that_does_not_take_offers_the_force() {
        let t0 = Instant::now();
        let asked = t0 + secs(10);

        let text = status_text(t0, Some((asked, false)), true, asked + STOP_TIMEOUT);

        assert_eq!(text, "not responding, press Ctrl+C again to force stop");
    }

    #[test]
    fn a_forced_stop_says_so_at_once() {
        let t0 = Instant::now();

        let text = status_text(t0, Some((t0, true)), true, t0 + secs(1));

        assert_eq!(text, "Forcing stop");
    }

    #[test]
    fn a_force_that_did_not_end_the_turn_offers_the_force_again() {
        let t0 = Instant::now();

        let text = status_text(t0, Some((t0, true)), true, t0 + STOP_TIMEOUT);

        assert_eq!(text, "not responding, press Ctrl+C again to force stop");
    }

    #[test]
    fn a_daemon_that_cannot_force_is_not_asked_to() {
        let t0 = Instant::now();

        let text = status_text(t0, Some((t0, false)), false, t0 + STOP_TIMEOUT);

        assert_eq!(text, "not responding");
    }

    #[test]
    fn a_clock_that_went_back_does_not_panic_or_offer_the_force() {
        let t0 = Instant::now();

        assert_eq!(stage(t0 + secs(5), false, t0), Stage::Stopping);
    }
}
