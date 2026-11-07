// SPDX-License-Identifier: Apache-2.0

//! The plan limits the provider last reported, kept for the status line and `/limits`.
//!
//! A report names the windows it knows, and Claude Code sends one at a time, so a
//! window stays until a later report replaces it or its reset time passes. Nothing here
//! asks the provider or a model: it only remembers what the events said.

use std::collections::BTreeMap;
use std::time::Duration;

use coda_protocol::events::{Event, RateLimitWindow};

use crate::errors::long_duration;

const BASIS_POINTS_PER_PERCENT: u32 = 100;
const FULL_PERCENT: u32 = 100;
/// A window is on pace while its use is within this many points of the share of time gone.
const PACE_TOLERANCE_POINTS: i64 = 5;
const FIVE_HOUR: &str = "five_hour";
const SEVEN_DAY: &str = "seven_day";
const MONTHLY: &str = "monthly";
const FIVE_HOUR_SECONDS: u64 = 5 * 3_600;
const SEVEN_DAY_SECONDS: u64 = 7 * 86_400;
const MONTH_SECONDS: u64 = 30 * 86_400;

/// How a window's use compares with the time that has gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// More than five points above the share of the window that has passed.
    Ahead,
    /// Within five points of it.
    OnPace,
    /// More than five points below it.
    Under,
}

impl Pace {
    fn text(self) -> &'static str {
        match self {
            Self::Ahead => "ahead",
            Self::OnPace => "on pace",
            Self::Under => "under",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    used_bps: u32,
    resets_at: Option<u64>,
}

/// The windows last reported, by the provider's name for them.
#[derive(Debug, Default, Clone)]
pub struct Limits {
    windows: BTreeMap<String, Window>,
}

impl Limits {
    /// Remembers the windows of a `rate_limit` event; other events change nothing.
    pub fn apply(&mut self, event: &Event) {
        if let Event::RateLimit { limit, .. } = event {
            self.remember(&limit.windows);
        }
    }

    /// The limits made of windows a daemon already summed.
    #[must_use]
    pub fn from_windows(windows: &[RateLimitWindow]) -> Self {
        let mut limits = Self::default();
        limits.remember(windows);
        limits
    }

    fn remember(&mut self, windows: &[RateLimitWindow]) {
        for window in windows {
            self.windows.insert(
                window.name.clone(),
                Window {
                    used_bps: window.utilization_bps,
                    resets_at: window.resets_at,
                },
            );
        }
    }

    /// The windows that have not reset yet, by name.
    fn live(&self, now: u64) -> impl Iterator<Item = (&String, &Window)> {
        self.windows
            .iter()
            .filter(move |(_, window)| window.resets_at.is_none_or(|at| at > now))
    }

    /// The most used window as one status line part, such as `5h 62% left, resets in 2h 15m, ahead`.
    #[must_use]
    pub fn status_part(&self, now: u64) -> Option<String> {
        let (name, window) = self.live(now).reduce(|best, next| {
            if next.1.used_bps > best.1.used_bps {
                next
            } else {
                best
            }
        })?;
        Some(describe(name, window, now))
    }

    /// One line per window that has not reset, for `/limits`.
    #[must_use]
    pub fn report(&self, now: u64) -> Vec<String> {
        self.live(now)
            .map(|(name, window)| describe(name, window, now))
            .collect()
    }
}

fn describe(name: &str, window: &Window, now: u64) -> String {
    let used = window.used_bps / BASIS_POINTS_PER_PERCENT;
    let left = FULL_PERCENT.saturating_sub(used);
    let mut text = format!("{} {left}% left", label(name));
    if let Some(at) = window.resets_at {
        let time = long_duration(Duration::from_secs(at.saturating_sub(now)));
        if !time.is_empty() {
            text.push_str(", resets in ");
            text.push_str(&time);
        }
        if let Some(pace) = pace(name, window.used_bps, at, now) {
            text.push_str(", ");
            text.push_str(pace.text());
        }
    }
    text
}

fn label(name: &str) -> String {
    match name {
        FIVE_HOUR => "5h".to_owned(),
        SEVEN_DAY => "7d".to_owned(),
        MONTHLY => "mo".to_owned(),
        other => other.replace('_', " "),
    }
}

fn window_seconds(name: &str) -> Option<u64> {
    match name {
        FIVE_HOUR => Some(FIVE_HOUR_SECONDS),
        SEVEN_DAY => Some(SEVEN_DAY_SECONDS),
        MONTHLY => Some(MONTH_SECONDS),
        _ => None,
    }
}

/// How a window's use compares with the time gone, when the window's length is known.
#[must_use]
pub fn pace(name: &str, used_bps: u32, resets_at: u64, now: u64) -> Option<Pace> {
    let length = window_seconds(name)?;
    let left = resets_at.saturating_sub(now).min(length);
    let elapsed_percent = i64::try_from((length - left) * u64::from(FULL_PERCENT) / length).ok()?;
    let used_percent = i64::from(used_bps / BASIS_POINTS_PER_PERCENT);
    let gap = used_percent - elapsed_percent;
    Some(if gap > PACE_TOLERANCE_POINTS {
        Pace::Ahead
    } else if gap < -PACE_TOLERANCE_POINTS {
        Pace::Under
    } else {
        Pace::OnPace
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_protocol::events::{RateLimit, RateLimitWindow};
    use coda_protocol::types::SessionId;

    const NOW: u64 = 1_000_000;

    fn report_of(windows: &[(&str, u32, Option<u64>)]) -> Event {
        Event::RateLimit {
            session_id: SessionId::new("s-1"),
            limit: RateLimit {
                status: "allowed".to_owned(),
                window_type: None,
                resets_at: None,
                windows: windows
                    .iter()
                    .map(|(name, bps, resets)| RateLimitWindow {
                        name: (*name).to_owned(),
                        utilization_bps: *bps,
                        resets_at: *resets,
                    })
                    .collect(),
                is_using_overage: false,
                overage_status: None,
                overage_resets_at: None,
                overage_disabled_reason: None,
            },
        }
    }

    /// A five-hour window with `hours_left` of its five to run, at `bps` use.
    fn five_hour(bps: u32, hours_left: u64) -> Event {
        report_of(&[(FIVE_HOUR, bps, Some(NOW + hours_left * 3600))])
    }

    #[test]
    fn the_status_part_names_what_is_left_when_it_resets_and_the_pace() {
        // Arrange
        let mut limits = Limits::default();

        // Act
        limits.apply(&five_hour(3800, 2));

        // Assert
        assert_eq!(
            limits.status_part(NOW).as_deref(),
            Some("5h 62% left, resets in 2h 00m, under")
        );
    }

    #[test]
    fn pace_is_ahead_on_pace_or_under_by_five_points_of_the_time_gone() {
        // Arrange: halfway through a five-hour window.
        let resets_at = NOW + 9_000;
        let at = |bps| pace(FIVE_HOUR, bps, resets_at, NOW);

        // Act and Assert
        assert_eq!(at(5600), Some(Pace::Ahead));
        assert_eq!(at(5500), Some(Pace::OnPace));
        assert_eq!(at(4500), Some(Pace::OnPace));
        assert_eq!(at(4400), Some(Pace::Under));
    }

    #[test]
    fn a_window_of_an_unknown_length_has_no_pace() {
        assert_eq!(pace("primary", 5000, NOW + 100, NOW), None);
        assert_eq!(pace("seven_day_opus", 5000, NOW + 100, NOW), None);
    }

    #[test]
    fn the_week_and_the_month_have_lengths_of_their_own() {
        let half_week = NOW + SEVEN_DAY_SECONDS / 2;
        let half_month = NOW + MONTH_SECONDS / 2;
        assert_eq!(pace(SEVEN_DAY, 5000, half_week, NOW), Some(Pace::OnPace));
        assert_eq!(pace(MONTHLY, 9000, half_month, NOW), Some(Pace::Ahead));
    }

    #[test]
    fn the_most_used_window_is_the_one_on_the_status_line() {
        // Arrange
        let mut limits = Limits::default();

        // Act
        limits.apply(&report_of(&[
            (FIVE_HOUR, 1000, Some(NOW + 3600)),
            (SEVEN_DAY, 8000, Some(NOW + 86_400)),
        ]));

        // Assert
        let part = limits.status_part(NOW).unwrap();
        assert!(part.starts_with("7d 20% left, resets in 1d 0h"), "{part}");
    }

    #[test]
    fn a_later_report_replaces_a_window_and_keeps_the_others() {
        // Arrange
        let mut limits = Limits::default();
        limits.apply(&report_of(&[(FIVE_HOUR, 1000, Some(NOW + 3600))]));
        limits.apply(&report_of(&[(SEVEN_DAY, 2000, Some(NOW + 86_400))]));

        // Act
        limits.apply(&report_of(&[(FIVE_HOUR, 9000, Some(NOW + 3600))]));

        // Assert
        let lines = limits.report(NOW);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("5h") && lines[0].contains("10% left"),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("7d") && lines[1].contains("80% left"),
            "{lines:?}"
        );
    }

    #[test]
    fn a_window_whose_reset_has_passed_is_dropped() {
        // Arrange
        let mut limits = Limits::default();
        limits.apply(&report_of(&[(FIVE_HOUR, 9000, Some(NOW + 60))]));

        // Act and Assert
        assert!(limits.status_part(NOW + 60).is_none());
        assert_eq!(limits.report(NOW + 61), Vec::<String>::new());
    }

    #[test]
    fn a_window_with_no_reset_time_stays_and_has_no_pace() {
        // Arrange
        let mut limits = Limits::default();
        limits.apply(&report_of(&[("primary", 4000, None)]));

        // Act
        let part = limits.status_part(NOW).unwrap();

        // Assert
        assert_eq!(part, "primary 60% left");
    }

    #[test]
    fn use_over_the_whole_window_reads_as_nothing_left() {
        // Arrange
        let mut limits = Limits::default();
        limits.apply(&report_of(&[(FIVE_HOUR, 12_000, Some(NOW + 3600))]));

        // Act
        let part = limits.status_part(NOW).unwrap();

        // Assert
        assert!(part.starts_with("5h 0% left"), "{part}");
    }

    #[test]
    fn events_that_are_not_reports_change_nothing() {
        // Arrange
        let mut limits = Limits::default();

        // Act
        limits.apply(&Event::Unknown);

        // Assert
        assert_eq!(limits.status_part(NOW), None);
        assert_eq!(limits.report(NOW), Vec::<String>::new());
    }

    #[test]
    fn a_provider_name_without_a_short_form_reads_with_spaces() {
        // Arrange
        let mut limits = Limits::default();
        limits.apply(&report_of(&[("seven_day_opus", 3000, Some(NOW + 3600))]));

        // Act
        let part = limits.status_part(NOW).unwrap();

        // Assert
        assert!(part.starts_with("seven day opus 70% left"), "{part}");
    }

    #[test]
    fn windows_a_daemon_summed_read_like_reported_ones() {
        // Arrange
        let window = RateLimitWindow {
            name: SEVEN_DAY.to_owned(),
            utilization_bps: 2500,
            resets_at: Some(NOW + 3600),
        };

        // Act
        let limits = Limits::from_windows(&[window]);

        // Assert
        let lines = limits.report(NOW);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].starts_with("7d 75% left, resets in 1h 00m"),
            "{lines:?}"
        );
    }
}
