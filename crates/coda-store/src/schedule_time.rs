// SPDX-License-Identifier: Apache-2.0

//! When a scheduled prompt runs next. Times are milliseconds since the Unix epoch and
//! days are UTC.

use coda_protocol::methods::{ScheduleSpec, Weekday};

const MS_PER_SECOND: i64 = 1000;
const MS_PER_MINUTE: i64 = 60 * MS_PER_SECOND;
const MS_PER_DAY: i64 = 24 * 60 * MS_PER_MINUTE;
const DAYS_PER_WEEK: i64 = 7;
/// 1970-01-01 was a Thursday, the fourth day with Monday as 0.
const EPOCH_WEEKDAY: i64 = 3;

/// A bit for each weekday, Monday as bit 0. Zero means every day.
#[must_use]
pub fn weekday_mask(days: &[Weekday]) -> u8 {
    days.iter().fold(0, |mask, day| mask | (1 << day.index()))
}

/// The weekdays a mask names, Monday first.
#[must_use]
pub fn weekdays_of(mask: u8) -> Vec<Weekday> {
    Weekday::ALL
        .into_iter()
        .filter(|day| mask & (1 << day.index()) != 0)
        .collect()
}

/// When a schedule first runs, if created at `now`.
#[must_use]
pub fn first_run(spec: &ScheduleSpec, now: i64) -> Option<i64> {
    match spec {
        ScheduleSpec::Once { at } => Some(*at),
        _ => next_after(spec, now),
    }
}

/// When a schedule runs after a run started at `now`; none for a one-time prompt.
#[must_use]
pub fn next_after(spec: &ScheduleSpec, now: i64) -> Option<i64> {
    match spec {
        ScheduleSpec::Once { .. } => None,
        ScheduleSpec::Interval { every_secs } => {
            let every = i64::try_from(*every_secs)
                .ok()?
                .checked_mul(MS_PER_SECOND)?;
            now.checked_add(every)
        }
        ScheduleSpec::Daily {
            minute_of_day,
            weekdays,
        } => {
            let mask = weekday_mask(weekdays);
            let first_day = now.div_euclid(MS_PER_DAY);
            (first_day..=first_day + DAYS_PER_WEEK).find_map(|day| {
                let at = day * MS_PER_DAY + i64::from(*minute_of_day) * MS_PER_MINUTE;
                let weekday =
                    usize::try_from((day + EPOCH_WEEKDAY).rem_euclid(DAYS_PER_WEEK)).ok()?;
                let allowed = mask == 0 || mask & (1 << weekday) != 0;
                (at > now && allowed).then_some(at)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2023-11-14 22:13:20 UTC, a Tuesday.
    const TUESDAY_EVENING: i64 = 1_700_000_000_000;
    /// 2023-11-14 00:00:00 UTC.
    const TUESDAY_START: i64 =
        TUESDAY_EVENING - (22 * 60 + 13) * MS_PER_MINUTE - 20 * MS_PER_SECOND;

    fn daily(minute_of_day: u32, weekdays: &[Weekday]) -> ScheduleSpec {
        ScheduleSpec::Daily {
            minute_of_day,
            weekdays: weekdays.to_vec(),
        }
    }

    #[test]
    fn a_one_time_prompt_runs_at_its_time_and_never_again() {
        // Arrange
        let spec = ScheduleSpec::Once { at: 5_000 };

        // Act and Assert
        assert_eq!(first_run(&spec, 0), Some(5_000));
        assert_eq!(next_after(&spec, 5_000), None);
    }

    #[test]
    fn an_interval_runs_one_interval_after_the_start() {
        // Arrange
        let spec = ScheduleSpec::Interval { every_secs: 90 };

        // Act and Assert
        assert_eq!(first_run(&spec, 1_000), Some(91_000));
        assert_eq!(next_after(&spec, 200_000), Some(290_000));
    }

    #[test]
    fn a_daily_prompt_runs_today_when_its_time_has_not_come() {
        // Arrange
        let spec = daily(23 * 60, &[]);

        // Act
        let next = first_run(&spec, TUESDAY_EVENING);

        // Assert
        assert_eq!(next, Some(TUESDAY_START + 23 * 60 * MS_PER_MINUTE));
    }

    #[test]
    fn a_daily_prompt_runs_tomorrow_when_its_time_has_passed() {
        // Arrange
        let spec = daily(9 * 60, &[]);

        // Act
        let next = first_run(&spec, TUESDAY_EVENING);

        // Assert
        assert_eq!(
            next,
            Some(TUESDAY_START + MS_PER_DAY + 9 * 60 * MS_PER_MINUTE)
        );
    }

    #[test]
    fn the_exact_minute_counts_as_passed() {
        // Arrange
        let at = TUESDAY_START + 9 * 60 * MS_PER_MINUTE;
        let spec = daily(9 * 60, &[]);

        // Act
        let next = next_after(&spec, at);

        // Assert
        assert_eq!(next, Some(at + MS_PER_DAY));
    }

    #[test]
    fn weekdays_pick_the_next_allowed_day() {
        // Arrange
        let friday = daily(9 * 60, &[Weekday::Fri]);
        let monday_or_tuesday = daily(23 * 60 + 30, &[Weekday::Mon, Weekday::Tue]);

        // Act
        let to_friday = first_run(&friday, TUESDAY_EVENING);
        let tonight = first_run(&monday_or_tuesday, TUESDAY_EVENING);

        // Assert
        assert_eq!(
            to_friday,
            Some(TUESDAY_START + 3 * MS_PER_DAY + 9 * 60 * MS_PER_MINUTE)
        );
        assert_eq!(
            tonight,
            Some(TUESDAY_START + (23 * 60 + 30) * MS_PER_MINUTE)
        );
    }

    #[test]
    fn a_single_weekday_whose_time_passed_waits_a_week() {
        // Arrange
        let spec = daily(9 * 60, &[Weekday::Tue]);

        // Act
        let next = first_run(&spec, TUESDAY_EVENING);

        // Assert
        assert_eq!(
            next,
            Some(TUESDAY_START + 7 * MS_PER_DAY + 9 * 60 * MS_PER_MINUTE)
        );
    }

    #[test]
    fn a_mask_round_trips_through_its_weekdays() {
        // Arrange
        let days = [Weekday::Mon, Weekday::Sat, Weekday::Sun];

        // Act
        let back = weekdays_of(weekday_mask(&days));

        // Assert
        assert_eq!(back, days);
        assert_eq!(weekday_mask(&[]), 0);
    }
}
