// SPDX-License-Identifier: Apache-2.0

//! Scheduled prompts: limits, claiming a due run and recovery after a crash.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use coda_protocol::methods::{MAX_SCHEDULES, ScheduleSpec, ScheduleState, ScheduleTarget, Weekday};
use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::{
    Clock, MAX_LISTED_SCHEDULES, MAX_SCHEDULE_ERROR_CHARS, MAX_SCHEDULE_INTERVAL_SECS,
    MAX_SCHEDULE_TEXT_BYTES, NewSession, Store, StoreError,
};
use tempfile::TempDir;

const START: i64 = 1_700_000_000_000;
const MINUTE_MS: i64 = 60_000;

#[derive(Debug, Default)]
struct Moving(AtomicI64);

impl Clock for Moving {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Rig {
    _dir: TempDir,
    store: Store,
    clock: Arc<Moving>,
    session: SessionId,
}

fn rig() -> Rig {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(Moving(AtomicI64::new(START)));
    let mut store = Store::open_with_clock(&dir.path().join("data"), clock.clone()).unwrap();
    let session = store
        .create_session(&NewSession {
            backend: BackendKind::Faux,
            model: None,
            cwd: "/repo".to_owned(),
            mode: Mode::Default,
        })
        .unwrap();
    Rig {
        _dir: dir,
        store,
        clock,
        session,
    }
}

fn into(session: &SessionId) -> ScheduleTarget {
    ScheduleTarget::Session {
        session_id: session.clone(),
    }
}

fn every(secs: u64) -> ScheduleSpec {
    ScheduleSpec::Interval { every_secs: secs }
}

fn advance(rig: &Rig, ms: i64) {
    rig.clock.0.fetch_add(ms, Ordering::SeqCst);
}

#[test]
fn an_interval_under_a_minute_is_refused() {
    // Arrange
    let mut rig = rig();

    // Act
    let refused = rig
        .store
        .add_schedule(&every(59), &into(&rig.session), "hi");

    // Assert
    assert!(
        matches!(refused, Err(StoreError::ScheduleRefused { .. })),
        "{refused:?}"
    );
}

#[test]
fn an_interval_of_a_minute_is_taken() {
    // Arrange
    let mut rig = rig();

    // Act
    let added = rig
        .store
        .add_schedule(&every(60), &into(&rig.session), "hi")
        .unwrap();

    // Assert
    assert_eq!(added.next_run_at, Some(START + MINUTE_MS));
    assert_eq!(added.state, ScheduleState::Idle);
    assert_eq!(added.runs, 0);
}

#[test]
fn an_empty_prompt_a_late_time_of_day_and_a_relative_directory_are_refused() {
    // Arrange
    let mut rig = rig();
    let daily = ScheduleSpec::Daily {
        minute_of_day: 1440,
        weekdays: Vec::new(),
    };
    let fresh = ScheduleTarget::NewSession {
        backend: BackendKind::Faux,
        model: None,
        cwd: "repo".to_owned(),
        mode: Mode::Default,
    };

    // Act
    let empty = rig
        .store
        .add_schedule(&every(60), &into(&rig.session), "  ");
    let late = rig.store.add_schedule(&daily, &into(&rig.session), "hi");
    let relative = rig.store.add_schedule(&every(60), &fresh, "hi");

    // Assert
    for refused in [empty, late, relative] {
        assert!(
            matches!(refused, Err(StoreError::ScheduleRefused { .. })),
            "{refused:?}"
        );
    }
}

#[test]
fn a_schedule_for_a_session_that_does_not_exist_is_refused() {
    // Arrange
    let mut rig = rig();

    // Act
    let refused = rig
        .store
        .add_schedule(&every(60), &into(&SessionId::new("999")), "hi");

    // Assert
    assert!(
        matches!(refused, Err(StoreError::SessionNotFound { .. })),
        "{refused:?}"
    );
}

#[test]
fn the_store_keeps_only_so_many_live_schedules() {
    // Arrange
    let mut rig = rig();
    for _ in 0..MAX_SCHEDULES {
        rig.store
            .add_schedule(&every(60), &into(&rig.session), "hi")
            .unwrap();
    }

    // Act
    let over = rig
        .store
        .add_schedule(&every(60), &into(&rig.session), "hi");
    let removed = rig.store.remove_schedule(1).unwrap();
    let again = rig
        .store
        .add_schedule(&every(60), &into(&rig.session), "hi");

    // Assert
    assert!(
        matches!(over, Err(StoreError::ScheduleRefused { .. })),
        "{over:?}"
    );
    assert!(removed);
    assert!(again.is_ok());
}

#[test]
fn a_new_session_target_and_a_daily_spec_read_back() {
    // Arrange
    let mut rig = rig();
    let spec = ScheduleSpec::Daily {
        minute_of_day: 540,
        weekdays: vec![Weekday::Mon, Weekday::Fri],
    };
    let target = ScheduleTarget::NewSession {
        backend: BackendKind::ClaudeCode,
        model: Some("opus".to_owned()),
        cwd: "/work/repo".to_owned(),
        mode: Mode::Plan,
    };
    rig.store.add_schedule(&spec, &target, "summarize").unwrap();

    // Act
    let listed = rig.store.list_schedules().unwrap();

    // Assert
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].spec, spec);
    assert_eq!(listed[0].target, target);
    assert_eq!(listed[0].text, "summarize");
}

#[test]
fn nothing_is_claimed_before_its_time_and_a_due_run_is_claimed_once() {
    // Arrange
    let mut rig = rig();
    rig.store
        .add_schedule(&every(60), &into(&rig.session), "hi")
        .unwrap();

    // Act
    let early = rig.store.claim_due_schedules(10).unwrap();
    advance(&rig, MINUTE_MS);
    let due = rig.store.claim_due_schedules(10).unwrap();
    let again = rig.store.claim_due_schedules(10).unwrap();

    // Assert
    assert_eq!(early.len(), 0);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].state, ScheduleState::Running);
    assert_eq!(due[0].runs, 1);
    assert_eq!(due[0].next_run_at, Some(START + 2 * MINUTE_MS));
    assert_eq!(again.len(), 0);
}

#[test]
fn a_running_prompt_is_not_claimed_again_until_it_finishes() {
    // Arrange
    let mut rig = rig();
    let added = rig
        .store
        .add_schedule(&every(60), &into(&rig.session), "hi")
        .unwrap();
    advance(&rig, MINUTE_MS);
    rig.store.claim_due_schedules(10).unwrap();
    advance(&rig, 5 * MINUTE_MS);

    // Act
    let while_running = rig.store.claim_due_schedules(10).unwrap();
    rig.store.finish_schedule_run(added.id).unwrap();
    let after = rig.store.claim_due_schedules(10).unwrap();

    // Assert
    assert_eq!(while_running.len(), 0);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].runs, 2);
}

#[test]
fn a_one_time_prompt_runs_once_and_is_done() {
    // Arrange
    let mut rig = rig();
    let added = rig
        .store
        .add_schedule(
            &ScheduleSpec::Once {
                at: START + MINUTE_MS,
            },
            &into(&rig.session),
            "hi",
        )
        .unwrap();
    advance(&rig, MINUTE_MS);

    // Act
    let due = rig.store.claim_due_schedules(10).unwrap();
    rig.store.finish_schedule_run(added.id).unwrap();
    advance(&rig, 60 * MINUTE_MS);
    let later = rig.store.claim_due_schedules(10).unwrap();

    // Assert
    assert_eq!(due.len(), 1);
    assert_eq!(later.len(), 0);
    let kept = rig.store.list_schedules().unwrap();
    assert_eq!(kept[0].state, ScheduleState::Done);
    assert_eq!(kept[0].next_run_at, None);
}

#[test]
fn a_tick_claims_at_most_the_limit_and_the_rest_wait_for_the_next() {
    // Arrange
    let mut rig = rig();
    for _ in 0..3 {
        rig.store
            .add_schedule(&every(60), &into(&rig.session), "hi")
            .unwrap();
    }
    advance(&rig, MINUTE_MS);

    // Act
    let first = rig.store.claim_due_schedules(2).unwrap();
    let second = rig.store.claim_due_schedules(2).unwrap();

    // Assert
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 1);
}

#[test]
fn recovery_puts_a_crashed_repeating_prompt_back_to_wait_for_its_next_run() {
    // Arrange
    let mut rig = rig();
    rig.store
        .add_schedule(&every(60), &into(&rig.session), "hi")
        .unwrap();
    advance(&rig, MINUTE_MS);
    rig.store.claim_due_schedules(10).unwrap();

    // Act
    let found = rig.store.recover_schedules().unwrap();
    let now = rig.store.claim_due_schedules(10).unwrap();
    advance(&rig, MINUTE_MS);
    let next = rig.store.claim_due_schedules(10).unwrap();

    // Assert
    assert_eq!(found, 1);
    assert_eq!(now.len(), 0);
    assert_eq!(next.len(), 1);
}

#[test]
fn recovery_runs_a_crashed_one_time_prompt_again_once() {
    // Arrange
    let mut rig = rig();
    rig.store
        .add_schedule(
            &ScheduleSpec::Once {
                at: START + MINUTE_MS,
            },
            &into(&rig.session),
            "hi",
        )
        .unwrap();
    advance(&rig, MINUTE_MS);
    rig.store.claim_due_schedules(10).unwrap();

    // Act
    rig.store.recover_schedules().unwrap();
    let again = rig.store.claim_due_schedules(10).unwrap();

    // Assert
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].runs, 1);
}

#[test]
fn recovery_with_nothing_left_running_finds_nothing() {
    // Arrange
    let mut rig = rig();
    rig.store
        .add_schedule(&every(60), &into(&rig.session), "hi")
        .unwrap();

    // Act
    let found = rig.store.recover_schedules().unwrap();

    // Assert
    assert_eq!(found, 0);
}

#[test]
fn a_removed_schedule_is_gone_and_removing_it_again_says_so() {
    // Arrange
    let mut rig = rig();
    rig.store
        .add_schedule(&every(60), &into(&rig.session), "hi")
        .unwrap();

    // Act
    let removed = rig.store.remove_schedule(1).unwrap();
    let missing = rig.store.remove_schedule(1).unwrap();

    // Assert
    assert!(removed);
    assert!(!missing);
    assert_eq!(rig.store.list_schedules().unwrap().len(), 0);
}

#[test]
fn an_interval_over_the_longest_is_refused() {
    // Arrange
    let mut rig = rig();

    // Act
    let refused = rig.store.add_schedule(
        &every(MAX_SCHEDULE_INTERVAL_SECS + 1),
        &into(&rig.session),
        "hi",
    );

    // Assert
    assert!(matches!(refused, Err(StoreError::ScheduleRefused { .. })));
}

#[test]
fn the_longest_interval_is_kept_with_a_next_run() {
    // Arrange
    let mut rig = rig();

    // Act
    let added = rig
        .store
        .add_schedule(
            &every(MAX_SCHEDULE_INTERVAL_SECS),
            &into(&rig.session),
            "hi",
        )
        .unwrap();

    // Assert
    assert!(added.next_run_at.is_some());
}

#[test]
fn a_prompt_over_the_size_limit_is_refused() {
    // Arrange
    let mut rig = rig();
    let text = "x".repeat(MAX_SCHEDULE_TEXT_BYTES + 1);

    // Act
    let refused = rig
        .store
        .add_schedule(&every(60), &into(&rig.session), &text);

    // Assert
    assert!(matches!(refused, Err(StoreError::ScheduleRefused { .. })));
}

#[test]
fn a_failed_run_leaves_its_reason_and_time_on_the_row_and_the_next_claim_clears_them() {
    // Arrange
    let mut rig = rig();
    let added = rig
        .store
        .add_schedule(&every(60), &into(&rig.session), "hi")
        .unwrap();
    advance(&rig, MINUTE_MS);
    rig.store.claim_due_schedules(10).unwrap();

    // Act
    rig.store
        .record_schedule_failure(added.id, "the folder is gone")
        .unwrap();
    rig.store.finish_schedule_run(added.id).unwrap();
    let failed = rig.store.list_schedules().unwrap().remove(0);
    advance(&rig, MINUTE_MS);
    rig.store.claim_due_schedules(10).unwrap();
    let retried = rig.store.list_schedules().unwrap().remove(0);

    // Assert
    assert_eq!(failed.last_error.as_deref(), Some("the folder is gone"));
    assert_eq!(failed.last_error_at, Some(START + MINUTE_MS));
    assert_eq!(failed.state, ScheduleState::Idle);
    assert_eq!(retried.last_error, None);
    assert_eq!(retried.last_error_at, None);
}

#[test]
fn a_failed_one_time_run_is_done_and_keeps_its_error_and_does_not_run_again() {
    // Arrange
    let mut rig = rig();
    let added = rig
        .store
        .add_schedule(
            &ScheduleSpec::Once {
                at: START + MINUTE_MS,
            },
            &into(&rig.session),
            "hi",
        )
        .unwrap();
    advance(&rig, MINUTE_MS);
    rig.store.claim_due_schedules(10).unwrap();

    // Act
    rig.store.record_schedule_failure(added.id, "down").unwrap();
    rig.store.finish_schedule_run(added.id).unwrap();
    advance(&rig, 60 * MINUTE_MS);
    let again = rig.store.claim_due_schedules(10).unwrap();

    // Assert
    assert_eq!(again.len(), 0);
    let kept = rig.store.list_schedules().unwrap().remove(0);
    assert_eq!(kept.state, ScheduleState::Done);
    assert_eq!(kept.last_error.as_deref(), Some("down"));
}

#[test]
fn a_long_error_is_cut_and_a_cut_never_splits_a_character() {
    // Arrange
    let mut rig = rig();
    let added = rig
        .store
        .add_schedule(&every(60), &into(&rig.session), "hi")
        .unwrap();
    let long = "\u{e9}".repeat(MAX_SCHEDULE_ERROR_CHARS * 2);

    // Act
    rig.store.record_schedule_failure(added.id, &long).unwrap();

    // Assert
    let kept = rig.store.list_schedules().unwrap().remove(0);
    assert_eq!(
        kept.last_error.unwrap().chars().count(),
        MAX_SCHEDULE_ERROR_CHARS
    );
}

#[test]
fn a_stopped_schedule_has_no_next_run_and_is_done_once_its_run_ends() {
    // Arrange
    let mut rig = rig();
    let added = rig
        .store
        .add_schedule(&every(60), &into(&rig.session), "hi")
        .unwrap();
    advance(&rig, MINUTE_MS);
    rig.store.claim_due_schedules(10).unwrap();

    // Act
    rig.store.stop_schedule(added.id).unwrap();
    rig.store.finish_schedule_run(added.id).unwrap();
    advance(&rig, 60 * MINUTE_MS);
    let again = rig.store.claim_due_schedules(10).unwrap();

    // Assert
    assert_eq!(again.len(), 0);
    let kept = rig.store.list_schedules().unwrap().remove(0);
    assert_eq!(kept.state, ScheduleState::Done);
    assert_eq!(kept.next_run_at, None);
}

#[test]
fn done_schedules_older_than_the_retention_are_pruned_and_live_ones_stay() {
    // Arrange
    let mut rig = rig();
    let old = rig
        .store
        .add_schedule(
            &ScheduleSpec::Once {
                at: START + MINUTE_MS,
            },
            &into(&rig.session),
            "old",
        )
        .unwrap();
    let live = rig
        .store
        .add_schedule(
            &every(MAX_SCHEDULE_INTERVAL_SECS),
            &into(&rig.session),
            "live",
        )
        .unwrap();
    advance(&rig, MINUTE_MS);
    rig.store.claim_due_schedules(10).unwrap();
    rig.store.finish_schedule_run(old.id).unwrap();
    let retention = 7 * 24 * 60 * MINUTE_MS;

    // Act
    advance(&rig, retention - 1);
    let early = rig.store.prune_done_schedules(retention).unwrap();
    advance(&rig, 2);
    let late = rig.store.prune_done_schedules(retention).unwrap();

    // Assert
    assert_eq!((early, late), (0, 1));
    let left: Vec<u64> = rig
        .store
        .list_schedules()
        .unwrap()
        .iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(left, [live.id]);
}

#[test]
fn the_list_holds_at_most_the_limit_and_keeps_the_newest() {
    // Arrange
    let mut rig = rig();
    let rounds = MAX_LISTED_SCHEDULES / MAX_SCHEDULES + 1;
    let mut last = 0;
    for _ in 0..rounds {
        for _ in 0..MAX_SCHEDULES {
            last = rig
                .store
                .add_schedule(
                    &ScheduleSpec::Once { at: START + 1 },
                    &into(&rig.session),
                    "x",
                )
                .unwrap()
                .id;
        }
        advance(&rig, MINUTE_MS);
        loop {
            let claimed = rig.store.claim_due_schedules(MAX_SCHEDULES).unwrap();
            if claimed.is_empty() {
                break;
            }
            for schedule in claimed {
                rig.store.finish_schedule_run(schedule.id).unwrap();
            }
        }
    }

    // Act
    let listed = rig.store.list_schedules().unwrap();

    // Assert
    assert_eq!(listed.len(), MAX_LISTED_SCHEDULES);
    assert_eq!(listed.last().unwrap().id, last);
    assert!(listed.windows(2).all(|pair| pair[0].id < pair[1].id));
}

#[test]
fn a_deleted_session_is_gone_and_a_second_delete_says_so() {
    // Arrange
    let mut rig = rig();

    // Act
    let first = rig.store.delete_session(&rig.session).unwrap();
    let second = rig.store.delete_session(&rig.session).unwrap();

    // Assert
    assert!(first);
    assert!(!second);
    assert!(rig.store.load_session(&rig.session).is_err());
}

#[test]
fn a_live_schedule_stays_in_the_list_when_many_newer_done_ones_exist() {
    // Arrange
    let mut rig = rig();
    let live = rig
        .store
        .add_schedule(
            &every(MAX_SCHEDULE_INTERVAL_SECS),
            &into(&rig.session),
            "live",
        )
        .unwrap();
    for _ in 0..=(MAX_LISTED_SCHEDULES / MAX_SCHEDULES) {
        for _ in 0..MAX_SCHEDULES - 1 {
            rig.store
                .add_schedule(
                    &ScheduleSpec::Once { at: START + 1 },
                    &into(&rig.session),
                    "x",
                )
                .unwrap();
        }
        advance(&rig, MINUTE_MS);
        loop {
            let claimed = rig.store.claim_due_schedules(MAX_SCHEDULES).unwrap();
            if claimed.is_empty() {
                break;
            }
            for schedule in claimed {
                rig.store.finish_schedule_run(schedule.id).unwrap();
            }
        }
    }

    // Act
    let listed = rig.store.list_schedules().unwrap();

    // Assert
    assert_eq!(listed.len(), MAX_LISTED_SCHEDULES);
    assert_eq!(listed[0].id, live.id);
    assert!(listed.windows(2).all(|pair| pair[0].id < pair[1].id));
}

#[test]
fn control_characters_in_an_error_are_replaced_by_spaces() {
    // Arrange
    let mut rig = rig();
    let added = rig
        .store
        .add_schedule(&every(60), &into(&rig.session), "hi")
        .unwrap();

    // Act
    rig.store
        .record_schedule_failure(added.id, "a\u{1b}[31mb\nc\td")
        .unwrap();

    // Assert
    let kept = rig.store.list_schedules().unwrap().remove(0);
    assert_eq!(kept.last_error.as_deref(), Some("a [31mb c d"));
}
