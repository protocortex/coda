// SPDX-License-Identifier: AGPL-3.0-only

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use coda_protocol::types::{BackendKind, Mode, SessionId, TurnId};
use coda_store::{Clock, EventRecord, NewSession, SessionFilter, Store, StoreError};
use rusqlite::Connection;
use tempfile::TempDir;

const T0: i64 = 1_700_000_000_000;
const V1_SNAPSHOT: &str = include_str!("snapshots/v1.sql");

#[derive(Debug)]
struct FixedClock(AtomicI64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn open_at(data_dir: &Path) -> Store {
    let clock = Arc::new(FixedClock(AtomicI64::new(T0)));
    Store::open_with_clock(data_dir, clock).unwrap()
}

fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = open_at(&dir.path().join("data"));
    (dir, store)
}

fn event(kind: &str, body: &str) -> EventRecord {
    EventRecord {
        kind: kind.to_owned(),
        body: body.to_owned(),
    }
}

fn new_session(cwd: &str) -> NewSession {
    NewSession {
        backend: BackendKind::Faux,
        model: None,
        cwd: cwd.to_owned(),
        mode: Mode::Default,
    }
}

#[test]
fn created_session_loads_back_with_the_same_fields() {
    // Arrange
    let (_dir, mut store) = open_store();
    let new = NewSession {
        backend: BackendKind::ClaudeCode,
        model: Some("opus".to_owned()),
        cwd: "/work/repo".to_owned(),
        mode: Mode::Plan,
    };

    // Act
    let id = store.create_session(&new).unwrap();
    let loaded = store.load_session(&id).unwrap();

    // Assert
    assert_eq!(loaded.id, id);
    assert_eq!(loaded.backend, BackendKind::ClaudeCode);
    assert_eq!(loaded.model.as_deref(), Some("opus"));
    assert_eq!(loaded.cwd, "/work/repo");
    assert_eq!(loaded.mode, Mode::Plan);
    assert_eq!(loaded.created_at, T0);
}

#[test]
fn listing_with_no_sessions_is_an_empty_vec() {
    // Arrange
    let (_dir, store) = open_store();

    // Act
    let listed = store.list_sessions(&SessionFilter::default()).unwrap();

    // Assert
    assert_eq!(listed, Vec::new());
}

#[test]
fn listing_returns_sessions_and_filters_by_cwd() {
    // Arrange
    let (_dir, mut store) = open_store();
    let a = store.create_session(&new_session("/a")).unwrap();
    let b = store.create_session(&new_session("/b")).unwrap();

    // Act
    let all = store.list_sessions(&SessionFilter::default()).unwrap();
    let only_b = store
        .list_sessions(&SessionFilter {
            cwd: Some("/b".to_owned()),
        })
        .unwrap();

    // Assert
    let all_ids: Vec<_> = all.into_iter().map(|s| s.id).collect();
    assert_eq!(all_ids, vec![a, b.clone()]);
    assert_eq!(only_b.len(), 1);
    assert_eq!(only_b[0].id, b);
}

#[test]
fn loading_a_nonexistent_session_is_session_not_found() {
    // Arrange
    let (_dir, store) = open_store();
    let missing = SessionId::new("404");

    // Act
    let err = store.load_session(&missing).unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::SessionNotFound { session } if session == missing));
}

#[test]
fn set_session_model_updates_the_stored_model() {
    // Arrange
    let (_dir, mut store) = open_store();
    let id = store.create_session(&new_session("/a")).unwrap();

    // Act
    store.set_session_model(&id, "sonnet").unwrap();

    // Assert
    assert_eq!(
        store.load_session(&id).unwrap().model.as_deref(),
        Some("sonnet")
    );
}

#[test]
fn setting_the_model_of_a_nonexistent_session_is_session_not_found() {
    // Arrange
    let (_dir, mut store) = open_store();

    // Act
    let err = store
        .set_session_model(&SessionId::new("7"), "sonnet")
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::SessionNotFound { .. }));
}

#[test]
fn appends_return_increasing_sequence_numbers_and_load_in_order() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();

    // Act
    let seqs: Vec<u64> = ["a", "b", "c"]
        .iter()
        .map(|body| {
            store
                .append_event(&session, &turn, &event("text", body))
                .unwrap()
        })
        .collect();
    let loaded = store.load_events(&session, 0, 10).unwrap();

    // Assert
    assert_eq!(seqs, vec![1, 2, 3]);
    let bodies: Vec<_> = loaded.iter().map(|e| (e.seq, e.body.as_str())).collect();
    assert_eq!(bodies, vec![(1, "a"), (2, "b"), (3, "c")]);
    assert_eq!(loaded[0].kind, "text");
    assert_eq!(loaded[0].turn, turn);
    assert_eq!(loaded[0].at, T0);
}

#[test]
fn sequence_numbers_continue_across_turns_of_a_session() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let first = store.begin_turn(&session).unwrap();
    let second = store.begin_turn(&session).unwrap();
    store
        .append_event(&session, &first, &event("k", "a"))
        .unwrap();

    // Act
    let seq = store
        .append_event(&session, &second, &event("k", "b"))
        .unwrap();

    // Assert
    assert_eq!(seq, 2);
}

#[test]
fn load_events_honours_after_seq_and_limit() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();
    for body in ["a", "b", "c", "d"] {
        store
            .append_event(&session, &turn, &event("k", body))
            .unwrap();
    }

    // Act
    let page = store.load_events(&session, 1, 2).unwrap();

    // Assert
    let seqs: Vec<_> = page.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![2, 3]);
}

#[test]
fn appending_to_a_nonexistent_session_is_session_not_found() {
    // Arrange
    let (_dir, mut store) = open_store();
    let missing = SessionId::new("9");

    // Act
    let err = store
        .append_event(&missing, &TurnId::new("1"), &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::SessionNotFound { .. }));
}

#[test]
fn appending_to_a_turn_of_another_session_is_turn_not_found() {
    // Arrange
    let (_dir, mut store) = open_store();
    let one = store.create_session(&new_session("/a")).unwrap();
    let two = store.create_session(&new_session("/b")).unwrap();
    let turn_of_two = store.begin_turn(&two).unwrap();

    // Act
    let err = store
        .append_event(&one, &turn_of_two, &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::TurnNotFound { .. }));
}

#[test]
fn appending_an_existing_sequence_number_is_a_duplicate_append() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();
    store
        .append_event_at(&session, &turn, 1, &event("k", "a"))
        .unwrap();

    // Act
    let err = store
        .append_event_at(&session, &turn, 1, &event("k", "again"))
        .unwrap_err();

    // Assert
    assert!(matches!(
        err,
        StoreError::DuplicateAppend { session: s, seq: 1 } if s == session
    ));
    assert_eq!(store.load_events(&session, 0, 10).unwrap().len(), 1);
}

#[test]
fn a_second_writer_on_the_same_session_is_rejected() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut first = open_at(&data_dir);
    let mut second = open_at(&data_dir);
    let session = first.create_session(&new_session("/a")).unwrap();
    let turn = first.begin_turn(&session).unwrap();
    first
        .append_event(&session, &turn, &event("k", "a"))
        .unwrap();

    // Act
    let err = second
        .append_event(&session, &turn, &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::WriterBusy { session: s } if s == session));
}

#[test]
fn dropping_the_writer_lets_another_handle_append() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut first = open_at(&data_dir);
    let mut second = open_at(&data_dir);
    let session = first.create_session(&new_session("/a")).unwrap();
    let turn = first.begin_turn(&session).unwrap();
    first
        .append_event(&session, &turn, &event("k", "a"))
        .unwrap();
    drop(first);

    // Act
    let seq = second
        .append_event(&session, &turn, &event("k", "b"))
        .unwrap();

    // Assert
    assert_eq!(seq, 2);
}

#[test]
fn a_database_failure_while_checking_the_session_is_a_sqlite_error() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut store = open_at(&data_dir);
    let session = store.create_session(&new_session("/a")).unwrap();
    Connection::open(data_dir.join("coda.db"))
        .unwrap()
        .execute_batch("DROP TABLE sessions")
        .unwrap();

    // Act
    let err = store.load_events(&session, 0, 10).unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::Sqlite(_)), "got {err:?}");
}

#[test]
fn a_database_failure_while_checking_the_turn_is_a_sqlite_error() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut store = open_at(&data_dir);
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();
    Connection::open(data_dir.join("coda.db"))
        .unwrap()
        .execute_batch("DROP TABLE turns")
        .unwrap();

    // Act
    let err = store
        .append_event(&session, &turn, &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::Sqlite(_)), "got {err:?}");
}

#[test]
fn a_non_unique_constraint_failure_is_not_a_duplicate_append() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut store = open_at(&data_dir);
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();
    Connection::open(data_dir.join("coda.db"))
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_events BEFORE INSERT ON events \
             BEGIN SELECT RAISE(ABORT, 'rejected'); END",
        )
        .unwrap();

    // Act
    let err = store
        .append_event_at(&session, &turn, 1, &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::Sqlite(_)), "got {err:?}");
}

#[test]
fn appending_at_sequence_zero_is_out_of_range() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();

    // Act
    let err = store
        .append_event_at(&session, &turn, 0, &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::ValueOutOfRange { value: 0 }));
    let stored = store.load_events(&session, 0, 10).unwrap();
    assert!(matches!(stored.as_slice(), []));
}

#[test]
fn a_failed_append_does_not_hold_the_writer_lease() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut first = open_at(&data_dir);
    let mut second = open_at(&data_dir);
    let session = first.create_session(&new_session("/a")).unwrap();
    let turn = first.begin_turn(&session).unwrap();
    let failed = first.append_event(&session, &TurnId::new("999"), &event("k", "a"));

    // Act
    let seq = second
        .append_event(&session, &turn, &event("k", "b"))
        .unwrap();

    // Assert
    assert!(matches!(failed, Err(StoreError::TurnNotFound { .. })));
    assert_eq!(seq, 1);
}

#[test]
fn a_v1_session_row_upgrades_with_the_v2_defaults() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let db_path = data_dir.join("coda.db");
    let old = Connection::open(&db_path).unwrap();
    old.execute_batch(V1_SNAPSHOT).unwrap();
    old.execute("INSERT INTO sessions (id, created_at) VALUES (7, ?1)", [T0])
        .unwrap();
    old.pragma_update(None, "user_version", 1).unwrap();
    drop(old);
    fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600)).unwrap();

    // Act
    let store = open_at(&data_dir);
    let loaded = store.load_session(&SessionId::new("7")).unwrap();

    // Assert
    assert_eq!(loaded.backend, BackendKind::Faux);
    assert_eq!(loaded.mode, Mode::Default);
    assert_eq!(loaded.cwd, "");
    assert_eq!(loaded.model, None);
    assert_eq!(loaded.created_at, T0);
}

#[test]
fn a_panic_while_holding_a_store_releases_the_writer_lease() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut first = open_at(&data_dir);
    let mut second = open_at(&data_dir);
    let session = first.create_session(&new_session("/a")).unwrap();
    let turn = first.begin_turn(&session).unwrap();

    // Act
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        first
            .append_event(&session, &turn, &event("k", "a"))
            .unwrap();
        panic!("simulated failure while holding the store");
    }));
    drop(first);
    let seq = second
        .append_event(&session, &turn, &event("k", "b"))
        .unwrap();

    // Assert
    assert!(outcome.is_err());
    assert_eq!(seq, 2);
}
