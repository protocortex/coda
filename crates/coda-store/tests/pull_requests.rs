// SPDX-License-Identifier: Apache-2.0

//! The pull requests linked to a session.

use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::{MAX_PULL_REQUESTS_PER_SESSION, NewSession, Store};
use tempfile::TempDir;

const FIRST: &str = "https://github.com/o/r/pull/1";
const SECOND: &str = "https://github.com/o/r/pull/2";

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

#[test]
fn a_new_session_has_no_pull_request() {
    // Arrange
    let (_dir, store, session) = open();

    // Act
    let found = store.session_pull_requests(&session).unwrap();

    // Assert
    assert_eq!(found, Vec::<String>::new());
}

#[test]
fn linked_pull_requests_read_back_newest_first() {
    // Arrange
    let (_dir, mut store, session) = open();
    store.link_pull_request(&session, FIRST).unwrap();
    store.link_pull_request(&session, SECOND).unwrap();

    // Act
    let found = store.session_pull_requests(&session).unwrap();

    // Assert
    assert_eq!(found, vec![SECOND.to_owned(), FIRST.to_owned()]);
}

#[test]
fn linking_the_same_pull_request_twice_keeps_one_row() {
    // Arrange
    let (_dir, mut store, session) = open();
    store.link_pull_request(&session, FIRST).unwrap();

    // Act
    store.link_pull_request(&session, FIRST).unwrap();

    // Assert
    assert_eq!(store.session_pull_requests(&session).unwrap().len(), 1);
}

#[test]
fn linking_to_a_session_that_does_not_exist_fails() {
    // Arrange
    let (_dir, mut store, _session) = open();

    // Act
    let linked = store.link_pull_request(&SessionId::new("999"), FIRST);

    // Assert
    assert!(linked.is_err());
}

#[test]
fn a_session_keeps_at_most_the_limit_of_pull_requests() {
    // Arrange
    let (_dir, mut store, session) = open();

    // Act
    for n in 0..MAX_PULL_REQUESTS_PER_SESSION + 5 {
        store
            .link_pull_request(&session, &format!("https://github.com/o/r/pull/{n}"))
            .unwrap();
    }

    // Assert
    let kept = store.session_pull_requests(&session).unwrap();
    assert_eq!(kept.len(), MAX_PULL_REQUESTS_PER_SESSION);
    assert!(kept.contains(&"https://github.com/o/r/pull/0".to_owned()));
    assert!(!kept.contains(&format!(
        "https://github.com/o/r/pull/{MAX_PULL_REQUESTS_PER_SESSION}"
    )));
}
