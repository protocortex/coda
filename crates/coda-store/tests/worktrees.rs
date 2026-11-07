// SPDX-License-Identifier: Apache-2.0

//! The worktree a session works in, recorded on its row.

use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::{NewSession, SessionWorktree, Store, StoreError};
use tempfile::TempDir;

fn open() -> (TempDir, Store, SessionId) {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let session = store
        .create_session(&NewSession {
            backend: BackendKind::ClaudeCode,
            model: None,
            cwd: "/repo".to_owned(),
            mode: Mode::Default,
        })
        .unwrap();
    (dir, store, session)
}

fn worktree() -> SessionWorktree {
    SessionWorktree {
        path: "/trees/1".to_owned(),
        branch: "coda/1".to_owned(),
        repo: "/repo".to_owned(),
    }
}

#[test]
fn a_new_session_has_no_worktree() {
    // Arrange
    let (_dir, store, session) = open();

    // Act
    let found = store.session_worktree(&session).unwrap();

    // Assert
    assert_eq!(found, None);
}

#[test]
fn a_recorded_worktree_is_read_back_and_becomes_the_working_directory() {
    // Arrange
    let (_dir, mut store, session) = open();

    // Act
    store.set_session_worktree(&session, &worktree()).unwrap();

    // Assert
    assert_eq!(store.session_worktree(&session).unwrap(), Some(worktree()));
    assert_eq!(store.load_session(&session).unwrap().cwd, "/trees/1");
}

#[test]
fn a_session_takes_a_worktree_once() {
    // Arrange
    let (_dir, mut store, session) = open();
    store.set_session_worktree(&session, &worktree()).unwrap();

    // Act
    let again = store.set_session_worktree(&session, &worktree());

    // Assert
    assert!(matches!(again, Err(StoreError::WorktreeRefused { .. })));
}

#[test]
fn a_session_with_a_turn_takes_no_worktree() {
    // Arrange
    let (_dir, mut store, session) = open();
    store.begin_turn(&session).unwrap();

    // Act
    let late = store.set_session_worktree(&session, &worktree());

    // Assert
    assert!(matches!(late, Err(StoreError::WorktreeRefused { .. })));
    assert_eq!(store.load_session(&session).unwrap().cwd, "/repo");
}

#[test]
fn an_unknown_session_is_not_found() {
    // Arrange
    let (_dir, mut store, _session) = open();
    let missing = SessionId::new("99");

    // Act
    let result = store.set_session_worktree(&missing, &worktree());

    // Assert
    assert!(matches!(result, Err(StoreError::SessionNotFound { .. })));
    assert_eq!(store.session_worktree(&missing).unwrap(), None);
}

#[test]
fn every_worktree_is_listed_with_its_session() {
    // Arrange
    let (_dir, mut store, session) = open();
    store.set_session_worktree(&session, &worktree()).unwrap();

    // Act
    let all = store.worktree_sessions().unwrap();

    // Assert
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].session, session);
    assert_eq!(all[0].worktree, worktree());
    assert_eq!(
        all[0].updated_at,
        store.load_session(&session).unwrap().created_at
    );
}
