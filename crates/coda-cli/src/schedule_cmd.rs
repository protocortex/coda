// SPDX-License-Identifier: Apache-2.0

//! `coda schedule add|list|remove`: prompts the daemon sends on a schedule.
//!
//! Times of day and weekdays are in UTC. A schedule only runs while the daemon runs.

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use coda_protocol::methods::{
    MIN_SCHEDULE_INTERVAL_SECS, SCHEDULE_ADD, SCHEDULE_LIST, SCHEDULE_REMOVE, ScheduleAddParams,
    ScheduleAddResult, ScheduleInfo, ScheduleListParams, ScheduleListResult, ScheduleRemoveParams,
    ScheduleRemoveResult, ScheduleSpec, ScheduleState, ScheduleTarget, Weekday,
};
use coda_protocol::types::SessionId;

use crate::autostart::{connect_unchecked, ensure_daemon};
use crate::cli::{ScheduleAddArgs, ScheduleCommand};
use crate::context::{Context, Io};
use crate::error::CliError;

const SECS_PER_MINUTE: u64 = 60;
const SECS_PER_HOUR: u64 = 60 * SECS_PER_MINUTE;
const SECS_PER_DAY: u64 = 24 * SECS_PER_HOUR;
const MS_PER_SECOND: i64 = 1000;
const MS_PER_MINUTE: i64 = 60_000;
const MINUTES_PER_HOUR: u32 = 60;
const HOURS_PER_DAY: u32 = 24;
const MS_PER_DAY: i64 = 86_400_000;
const NEVER: &str = "never";

/// Runs one `coda schedule` command.
///
/// # Errors
/// Returns the errors of reaching the daemon and of its request, and
/// [`CliError::Schedule`] for a time that cannot be read.
pub async fn run(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    command: ScheduleCommand,
) -> Result<ExitCode, CliError> {
    match command {
        ScheduleCommand::Add(args) => add(ctx, io, force, args).await?,
        ScheduleCommand::List(args) => list(ctx, io, args.json).await?,
        ScheduleCommand::Remove(args) => remove(ctx, io, args.id).await?,
    }
    Ok(ExitCode::SUCCESS)
}

fn bad(reason: impl Into<String>) -> CliError {
    CliError::Schedule(reason.into())
}

/// A length of time such as `90s`, `10m`, `2h` or `1d`, in seconds.
pub(crate) fn parse_duration(text: &str) -> Result<u64, CliError> {
    let text = text.trim();
    let split = text.char_indices().next_back().map_or(0, |(at, _)| at);
    let (number, unit) = text.split_at(split);
    let each = match unit {
        "s" => 1,
        "m" => SECS_PER_MINUTE,
        "h" => SECS_PER_HOUR,
        "d" => SECS_PER_DAY,
        _ => {
            return Err(bad(format!(
                "`{text}` is not a time such as 90s, 10m, 2h or 1d"
            )));
        }
    };
    let count: u64 = number
        .parse()
        .map_err(|_| bad(format!("`{text}` is not a time such as 90s, 10m, 2h or 1d")))?;
    count
        .checked_mul(each)
        .ok_or_else(|| bad(format!("`{text}` is too long")))
}

/// A time of day written `HH:MM`, as minutes after midnight.
pub(crate) fn parse_time_of_day(text: &str) -> Result<u32, CliError> {
    let wrong = || bad(format!("`{text}` is not a time of day such as 09:30"));
    let (hours, minutes) = text.split_once(':').ok_or_else(wrong)?;
    let (hours, minutes): (u32, u32) = (
        hours.parse().map_err(|_| wrong())?,
        minutes.parse().map_err(|_| wrong())?,
    );
    if hours >= HOURS_PER_DAY || minutes >= MINUTES_PER_HOUR {
        return Err(wrong());
    }
    Ok(hours * MINUTES_PER_HOUR + minutes)
}

/// Weekdays written as `mon,fri`.
pub(crate) fn parse_weekdays(text: &str) -> Result<Vec<Weekday>, CliError> {
    text.split(',')
        .map(|name| {
            let name = name.trim().to_ascii_lowercase();
            Weekday::ALL
                .into_iter()
                .find(|day| {
                    serde_json::to_value(day)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .is_some_and(|label| label == name)
                })
                .ok_or_else(|| bad(format!("`{name}` is not a weekday such as mon or fri")))
        })
        .collect()
}

fn now_ms() -> i64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
}

fn spec_of(args: &ScheduleAddArgs, now: i64) -> Result<ScheduleSpec, CliError> {
    match (&args.once_in, &args.every, &args.daily) {
        (Some(wait), None, None) => {
            let secs = i64::try_from(parse_duration(wait)?).map_err(|_| bad("too long"))?;
            Ok(ScheduleSpec::Once {
                at: now.saturating_add(secs.saturating_mul(MS_PER_SECOND)),
            })
        }
        (None, Some(every), None) => {
            let every_secs = parse_duration(every)?;
            if every_secs < MIN_SCHEDULE_INTERVAL_SECS {
                return Err(bad(format!(
                    "an interval must be at least {MIN_SCHEDULE_INTERVAL_SECS} seconds"
                )));
            }
            Ok(ScheduleSpec::Interval { every_secs })
        }
        (None, None, Some(time)) => Ok(ScheduleSpec::Daily {
            minute_of_day: parse_time_of_day(time)?,
            weekdays: args
                .weekdays
                .as_deref()
                .map(parse_weekdays)
                .transpose()?
                .unwrap_or_default(),
        }),
        _ => Err(bad("give exactly one of --in, --every and --daily")),
    }
}

fn target_of(ctx: &Context, args: &ScheduleAddArgs) -> Result<ScheduleTarget, CliError> {
    match (&args.session, &args.cwd) {
        (Some(session), None) => Ok(ScheduleTarget::Session {
            session_id: SessionId::new(session.clone()),
        }),
        (None, Some(cwd)) => Ok(ScheduleTarget::NewSession {
            backend: args
                .backend
                .map(Into::into)
                .ok_or_else(|| bad("--cwd needs --backend"))?,
            model: args.model.clone(),
            cwd: std::fs::canonicalize(ctx.cwd.join(cwd))
                .map_err(|e| {
                    bad(format!(
                        "`{}` is not a directory I can use: {e}",
                        cwd.display()
                    ))
                })?
                .display()
                .to_string(),
            mode: args.mode.into(),
        }),
        _ => Err(bad("give exactly one of --session and --cwd")),
    }
}

async fn add(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    args: ScheduleAddArgs,
) -> Result<(), CliError> {
    let params = ScheduleAddParams {
        spec: spec_of(&args, now_ms())?,
        target: target_of(ctx, &args)?,
        text: args.prompt.clone(),
    };
    let client = ensure_daemon(ctx, io, force).await?;
    let done: ScheduleAddResult = client.request(SCHEDULE_ADD, &params).await?;
    if args.json {
        writeln!(io.out, "{}", serde_json::to_string(&done.schedule)?)?;
    } else {
        writeln!(io.out, "{}", line(&done.schedule))?;
    }
    Ok(())
}

async fn list(ctx: &Context, io: &mut Io<'_>, json: bool) -> Result<(), CliError> {
    let client = connect_unchecked(ctx).await?;
    let listed: ScheduleListResult = client
        .request(SCHEDULE_LIST, &ScheduleListParams {})
        .await?;
    if json {
        writeln!(io.out, "{}", serde_json::to_string(&listed.schedules)?)?;
    } else {
        for schedule in &listed.schedules {
            writeln!(io.out, "{}", line(schedule))?;
        }
    }
    Ok(())
}

async fn remove(ctx: &Context, io: &mut Io<'_>, id: u64) -> Result<(), CliError> {
    let client = connect_unchecked(ctx).await?;
    let done: ScheduleRemoveResult = client
        .request(SCHEDULE_REMOVE, &ScheduleRemoveParams { id })
        .await?;
    if done.removed {
        writeln!(io.out, "removed {id}")?;
        Ok(())
    } else {
        Err(bad(format!("no scheduled prompt has the id {id}")))
    }
}

/// One line about a scheduled prompt: id, state, when it runs next, how often, where it goes.
pub(crate) fn line(schedule: &ScheduleInfo) -> String {
    let state = match schedule.state {
        ScheduleState::Idle => "idle",
        ScheduleState::Running => "running",
        ScheduleState::Done => "done",
    };
    let next = schedule
        .next_run_at
        .map_or_else(|| NEVER.to_owned(), format_utc);
    let target = match &schedule.target {
        ScheduleTarget::Session { session_id } => format!("session {session_id}"),
        ScheduleTarget::NewSession { cwd, .. } => format!("new session in {cwd}"),
    };
    format!(
        "{}  {state}  next {next}  {}  runs {}  {target}  {}",
        schedule.id,
        describe(&schedule.spec),
        schedule.runs,
        schedule.text.lines().next().unwrap_or_default()
    )
}

fn describe(spec: &ScheduleSpec) -> String {
    match spec {
        ScheduleSpec::Once { .. } => "once".to_owned(),
        ScheduleSpec::Interval { every_secs } => format!("every {every_secs}s"),
        ScheduleSpec::Daily {
            minute_of_day,
            weekdays,
        } => {
            let days = if weekdays.is_empty() {
                "every day".to_owned()
            } else {
                weekdays
                    .iter()
                    .filter_map(|day| serde_json::to_value(day).ok())
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            format!(
                "daily {:02}:{:02} UTC {days}",
                minute_of_day / MINUTES_PER_HOUR,
                minute_of_day % MINUTES_PER_HOUR
            )
        }
    }
}

/// `YYYY-MM-DD HH:MM UTC` for a time in milliseconds since the Unix epoch.
pub(crate) fn format_utc(ms: i64) -> String {
    let days = ms.div_euclid(MS_PER_DAY);
    let minutes = ms.rem_euclid(MS_PER_DAY) / MS_PER_MINUTE;
    // Civil date from a day count, for the proleptic Gregorian calendar.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = if month_part < 10 {
        month_part + 3
    } else {
        month_part - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        minutes / i64::from(MINUTES_PER_HOUR),
        minutes % i64::from(MINUTES_PER_HOUR)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_in_seconds_minutes_hours_and_days() {
        // Arrange and Act and Assert
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("10m").unwrap(), 600);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1d").unwrap(), 86_400);
    }

    #[test]
    fn a_duration_without_a_known_unit_or_a_number_is_refused() {
        for text in [
            "",
            "10",
            "m",
            "-5m",
            "1.5h",
            "5w",
            "5é",
            "18446744073709551615d",
        ] {
            // Arrange and Act
            let read = parse_duration(text);

            // Assert
            assert!(read.is_err(), "{text:?}");
        }
    }

    #[test]
    fn a_time_of_day_reads_as_minutes_after_midnight() {
        // Arrange and Act and Assert
        assert_eq!(parse_time_of_day("00:00").unwrap(), 0);
        assert_eq!(parse_time_of_day("09:30").unwrap(), 570);
        assert_eq!(parse_time_of_day("23:59").unwrap(), 1439);
        for bad in ["24:00", "12:60", "9", "ab:cd", ""] {
            assert!(parse_time_of_day(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn weekdays_read_by_their_short_names_in_any_case() {
        // Arrange and Act
        let days = parse_weekdays("Mon, fri,SUN").unwrap();

        // Assert
        assert_eq!(days, [Weekday::Mon, Weekday::Fri, Weekday::Sun]);
        assert!(parse_weekdays("monday").is_err());
    }

    #[test]
    fn a_time_is_written_in_utc() {
        // Arrange and Act and Assert
        assert_eq!(format_utc(1_700_000_000_000), "2023-11-14 22:13 UTC");
        assert_eq!(format_utc(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29 00:00 UTC");
    }
}
