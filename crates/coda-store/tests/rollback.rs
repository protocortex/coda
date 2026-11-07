// SPDX-License-Identifier: Apache-2.0

//! Turns a rewind marks as rolled back stay stored but leave the conversation.

use std::collections::HashSet;

use coda_protocol::types::{BackendKind, Mode, SessionId, TurnId};
use coda_store::{NewSession, SCHEMA_VERSION, Store, StoreError, migrate};
use rusqlite::Connection;
use tempfile::TempDir;

const V14_SNAPSHOT: &str = include_str!("snapshots/v14.sql");
const BEFORE_ROLLBACK: u32 = 14;

fn new_session(store: &mut Store) -> SessionId {
    store
        .create_session(&NewSession {
            backend: BackendKind::ClaudeCode,
            model: None,
            cwd: "/work".to_owned(),
            mode: Mode::Default,
        })
        .unwrap()
}

fn master_listing(conn: &Connection) -> Vec<(String, String, Option<String>)> {
    let mut stmt = conn
        .prepare(
            "SELECT type, name, sql FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn three_turns() -> (TempDir, Store, SessionId, [TurnId; 3]) {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let session = new_session(&mut store);
    let turns = [
        store.begin_turn(&session).unwrap(),
        store.begin_turn(&session).unwrap(),
        store.begin_turn(&session).unwrap(),
    ];
    (dir, store, session, turns)
}

fn set(turns: &[&TurnId]) -> HashSet<TurnId> {
    turns.iter().map(|t| (*t).clone()).collect()
}

#[test]
fn no_turn_is_rolled_back_until_a_rewind_marks_it() {
    // Arrange
    let (_dir, store, session, _turns) = three_turns();

    // Act
    let found = store.rolled_back_turns(&session).unwrap();

    // Assert
    assert!(found.is_empty());
}

#[test]
fn rolling_back_after_a_turn_marks_only_the_later_turns() {
    // Arrange
    let (_dir, mut store, session, [first, second, third]) = three_turns();

    // Act
    let marked = store.roll_back_turns_after(&session, Some(&first)).unwrap();

    // Assert
    assert_eq!(marked, 2);
    assert_eq!(
        store.rolled_back_turns(&session).unwrap(),
        set(&[&second, &third])
    );
}

#[test]
fn rolling_back_to_before_the_first_turn_marks_every_turn() {
    // Arrange
    let (_dir, mut store, session, [first, second, third]) = three_turns();

    // Act
    let marked = store.roll_back_turns_after(&session, None).unwrap();

    // Assert
    assert_eq!(marked, 3);
    assert_eq!(
        store.rolled_back_turns(&session).unwrap(),
        set(&[&first, &second, &third])
    );
}

#[test]
fn a_turn_already_rolled_back_is_not_counted_again() {
    // Arrange
    let (_dir, mut store, session, [first, _second, _third]) = three_turns();
    store.roll_back_turns_after(&session, Some(&first)).unwrap();

    // Act
    let marked = store.roll_back_turns_after(&session, Some(&first)).unwrap();

    // Assert
    assert_eq!(marked, 0);
}

#[test]
fn another_sessions_turn_cannot_be_the_target() {
    // Arrange
    let (_dir, mut store, session, _turns) = three_turns();
    let other = new_session(&mut store);
    let foreign = store.begin_turn(&other).unwrap();

    // Act
    let result = store.roll_back_turns_after(&session, Some(&foreign));

    // Assert
    assert!(matches!(result, Err(StoreError::TurnNotFound { .. })));
    assert!(store.rolled_back_turns(&session).unwrap().is_empty());
}

#[test]
fn another_sessions_turns_are_left_alone() {
    // Arrange
    let (_dir, mut store, session, _turns) = three_turns();
    let other = new_session(&mut store);
    store.begin_turn(&other).unwrap();

    // Act
    store.roll_back_turns_after(&session, None).unwrap();

    // Assert
    assert!(store.rolled_back_turns(&other).unwrap().is_empty());
}

#[test]
fn v14_snapshot_migrates_forward_with_no_turn_rolled_back() {
    // Arrange
    let mut old = Connection::open_in_memory().unwrap();
    old.execute_batch(V14_SNAPSHOT).unwrap();
    old.pragma_update(None, "user_version", BEFORE_ROLLBACK)
        .unwrap();
    old.execute_batch(
        "INSERT INTO sessions (id, created_at) VALUES (1, 0); \
         INSERT INTO turns (id, session_id, started_at) VALUES (1, 1, 0);",
    )
    .unwrap();

    let mut fresh = Connection::open_in_memory().unwrap();
    migrate::migrate(&mut fresh).unwrap();

    // Act
    migrate::migrate(&mut old).unwrap();

    // Assert
    assert_eq!(master_listing(&old), master_listing(&fresh));
    let version: u32 = old
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let marked: Option<i64> = old
        .query_row("SELECT rolled_back_at FROM turns WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(marked, None);
}
