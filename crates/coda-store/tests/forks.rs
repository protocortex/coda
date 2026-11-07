// SPDX-License-Identifier: Apache-2.0

//! A fork is a new session that remembers the session and turn it came from.

use std::collections::HashSet;

use coda_protocol::types::{BackendKind, Mode, SessionId, TurnId};
use coda_store::{NewSession, SessionFilter, Store, StoreError};
use tempfile::TempDir;

const MODEL: &str = "haiku";
const CWD: &str = "/work";

fn open() -> (TempDir, Store, SessionId) {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let parent = store
        .create_session(&NewSession {
            backend: BackendKind::ClaudeCode,
            model: Some(MODEL.to_owned()),
            cwd: CWD.to_owned(),
            mode: Mode::Plan,
        })
        .unwrap();
    (dir, store, parent)
}

fn begin_turn(store: &mut Store, session: &SessionId) -> TurnId {
    store.begin_turn(session).unwrap()
}

#[test]
fn a_fork_copies_the_settings_of_its_parent_and_names_where_it_came_from() {
    // Arrange
    let (_dir, mut store, parent) = open();
    let turn = begin_turn(&mut store, &parent);

    // Act
    let fork = store.create_fork(&parent, &turn).unwrap();

    // Assert
    let record = store.load_session(&fork).unwrap();
    assert_ne!(fork, parent);
    assert_eq!(record.backend, BackendKind::ClaudeCode);
    assert_eq!(record.model.as_deref(), Some(MODEL));
    assert_eq!(record.cwd, CWD);
    assert_eq!(record.mode, Mode::Plan);
    let link = store.fork_of(&fork).unwrap().unwrap();
    assert_eq!(link.parent, parent);
    assert_eq!(link.source_turn, turn);
}

#[test]
fn a_session_that_is_not_a_fork_has_no_parent() {
    // Arrange
    let (_dir, store, parent) = open();

    // Act
    let link = store.fork_of(&parent).unwrap();

    // Assert
    assert_eq!(link, None);
}

#[test]
fn the_link_is_read_back_after_the_store_is_reopened() {
    // Arrange
    let (dir, mut store, parent) = open();
    let turn = begin_turn(&mut store, &parent);
    let fork = store.create_fork(&parent, &turn).unwrap();
    drop(store);

    // Act
    let store = Store::open(&dir.path().join("data")).unwrap();

    // Assert
    assert_eq!(store.fork_of(&fork).unwrap().unwrap().parent, parent);
}

#[test]
fn a_turn_of_another_session_cannot_be_a_fork_point_and_no_session_is_made() {
    // Arrange
    let (_dir, mut store, parent) = open();
    let other = store
        .create_session(&NewSession {
            backend: BackendKind::ClaudeCode,
            model: None,
            cwd: CWD.to_owned(),
            mode: Mode::Default,
        })
        .unwrap();
    let foreign = begin_turn(&mut store, &other);
    let before = store
        .list_sessions(&SessionFilter::default())
        .unwrap()
        .len();

    // Act
    let refused = store.create_fork(&parent, &foreign);

    // Assert
    assert!(matches!(refused, Err(StoreError::TurnNotFound { .. })));
    assert_eq!(
        store
            .list_sessions(&SessionFilter::default())
            .unwrap()
            .len(),
        before
    );
}

#[test]
fn a_rolled_back_turn_cannot_be_a_fork_point() {
    // Arrange
    let (_dir, mut store, parent) = open();
    let first = begin_turn(&mut store, &parent);
    let second = begin_turn(&mut store, &parent);
    store.roll_back_turns_after(&parent, Some(&first)).unwrap();

    // Act
    let refused = store.create_fork(&parent, &second);

    // Assert
    assert!(matches!(refused, Err(StoreError::TurnRolledBack { .. })));
}

#[test]
fn a_fork_keeps_the_turns_its_parent_had_rolled_back_when_it_was_made() {
    // Arrange
    let (_dir, mut store, parent) = open();
    let first = begin_turn(&mut store, &parent);
    let second = begin_turn(&mut store, &parent);
    store.roll_back_turns_after(&parent, Some(&first)).unwrap();
    let third = begin_turn(&mut store, &parent);

    // Act
    let fork = store.create_fork(&parent, &third).unwrap();

    // Assert
    let link = store.fork_of(&fork).unwrap().unwrap();
    assert_eq!(link.rolled_back, HashSet::from([second]));
}

#[test]
fn a_later_rewind_of_the_parent_does_not_change_what_the_fork_kept() {
    // Arrange
    let (_dir, mut store, parent) = open();
    let first = begin_turn(&mut store, &parent);
    let second = begin_turn(&mut store, &parent);
    let fork = store.create_fork(&parent, &second).unwrap();

    // Act
    store.roll_back_turns_after(&parent, Some(&first)).unwrap();

    // Assert
    assert!(
        store
            .fork_of(&fork)
            .unwrap()
            .unwrap()
            .rolled_back
            .is_empty()
    );
}
