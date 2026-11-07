// SPDX-License-Identifier: Apache-2.0

//! The checkpoint refs kept on turn rows.

use coda_protocol::types::{BackendKind, Mode, SessionId, TurnId};
use coda_store::{NewSession, SCHEMA_VERSION, Store, StoreError, TurnCheckpoint, migrate};
use rusqlite::Connection;
use tempfile::TempDir;

const V12_SNAPSHOT: &str = include_str!("snapshots/v12.sql");
const BEFORE_CHECKPOINTS: u32 = 12;
const BEFORE_REF: &str = "refs/coda/checkpoints/1/base";
const AFTER_REF: &str = "refs/coda/checkpoints/1/1";

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

fn open() -> (TempDir, Store, SessionId) {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let session = new_session(&mut store);
    (dir, store, session)
}

#[test]
fn clearing_refs_forgets_only_those_refs() {
    // Arrange
    let (_dir, mut store, session) = open();
    let first = store.begin_turn(&session).unwrap();
    store
        .record_checkpoint_before(&session, &first, BEFORE_REF)
        .unwrap();
    store
        .record_checkpoint_after(&session, &first, AFTER_REF)
        .unwrap();

    // Act
    let cleared = store
        .clear_checkpoint_refs(&[
            AFTER_REF.to_owned(),
            "refs/coda/checkpoints/1/other".to_owned(),
        ])
        .unwrap();

    // Assert
    assert_eq!(cleared, 1);
    let found = store.turn_checkpoints(&session).unwrap();
    assert_eq!(found[0].before.as_deref(), Some(BEFORE_REF));
    assert_eq!(found[0].after, None);
}

#[test]
fn a_turn_has_no_checkpoint_until_one_is_recorded() {
    // Arrange
    let (_dir, mut store, session) = open();
    let turn = store.begin_turn(&session).unwrap();

    // Act
    let found = store.turn_checkpoints(&session).unwrap();

    // Assert
    assert_eq!(
        found,
        vec![TurnCheckpoint {
            turn,
            before: None,
            after: None
        }]
    );
}

#[test]
fn both_refs_are_read_back_per_turn_oldest_first() {
    // Arrange
    let (_dir, mut store, session) = open();
    let first = store.begin_turn(&session).unwrap();
    let second = store.begin_turn(&session).unwrap();
    store
        .record_checkpoint_before(&session, &first, BEFORE_REF)
        .unwrap();
    store
        .record_checkpoint_after(&session, &first, AFTER_REF)
        .unwrap();

    // Act
    let found = store.turn_checkpoints(&session).unwrap();

    // Assert
    assert_eq!(
        found,
        vec![
            TurnCheckpoint {
                turn: first,
                before: Some(BEFORE_REF.to_owned()),
                after: Some(AFTER_REF.to_owned()),
            },
            TurnCheckpoint {
                turn: second,
                before: None,
                after: None,
            },
        ]
    );
}

#[test]
fn another_sessions_turn_is_refused() {
    // Arrange
    let (_dir, mut store, session) = open();
    let other = new_session(&mut store);
    let turn = store.begin_turn(&other).unwrap();

    // Act
    let result = store.record_checkpoint_after(&session, &turn, AFTER_REF);

    // Assert
    assert!(matches!(result, Err(StoreError::TurnNotFound { .. })));
}

#[test]
fn an_unknown_turn_is_refused() {
    // Arrange
    let (_dir, mut store, session) = open();

    // Act
    let result = store.record_checkpoint_before(&session, &TurnId::new("99"), BEFORE_REF);

    // Assert
    assert!(matches!(result, Err(StoreError::TurnNotFound { .. })));
}

#[test]
fn v12_snapshot_migrates_forward_keeping_its_turns_without_a_checkpoint() {
    // Arrange
    let mut old = Connection::open_in_memory().unwrap();
    old.execute_batch(V12_SNAPSHOT).unwrap();
    old.pragma_update(None, "user_version", BEFORE_CHECKPOINTS)
        .unwrap();
    old.execute_batch(
        "INSERT INTO sessions (id, created_at) VALUES (1, 0); \
         INSERT INTO turns (id, session_id, started_at) VALUES (1, 1, 0);",
    )
    .unwrap();

    // Act
    migrate::migrate(&mut old).unwrap();

    // Assert
    let version: u32 = old
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let refs: (Option<String>, Option<String>) = old
        .query_row(
            "SELECT checkpoint_before, checkpoint_after FROM turns WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(refs, (None, None));
}
