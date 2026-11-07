// SPDX-License-Identifier: Apache-2.0

//! Follow-up messages kept for a session while its turn runs.

use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::{
    MAX_QUEUED_MESSAGES, MAX_QUEUED_TEXT_BYTES, NewQueued, NewSession, Store, StoreError,
};
use tempfile::TempDir;

fn plain(text: &str) -> NewQueued<'_> {
    NewQueued {
        text,
        max_turns: None,
        token_cap: None,
    }
}

fn new_session(store: &mut Store) -> SessionId {
    store
        .create_session(&NewSession {
            backend: BackendKind::Faux,
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
fn messages_come_back_oldest_first() {
    // Arrange
    let (_dir, mut store, session) = open();

    // Act
    store.enqueue_message(&session, plain("first")).unwrap();
    store.enqueue_message(&session, plain("second")).unwrap();

    // Assert
    let texts: Vec<_> = store
        .queued_messages(&session)
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(texts, ["first", "second"]);
}

#[test]
fn a_removed_message_is_gone_and_a_second_removal_says_so() {
    // Arrange
    let (_dir, mut store, session) = open();
    let kept = store.enqueue_message(&session, plain("keep")).unwrap();
    let dropped = store.enqueue_message(&session, plain("drop")).unwrap();

    // Act
    let first = store.remove_queued_message(&session, dropped.id).unwrap();
    let again = store.remove_queued_message(&session, dropped.id).unwrap();

    // Assert
    assert!(first);
    assert!(!again);
    assert_eq!(store.queued_messages(&session).unwrap(), [kept]);
}

#[test]
fn a_message_of_another_session_is_not_removed() {
    // Arrange
    let (_dir, mut store, session) = open();
    let other = new_session(&mut store);
    let message = store.enqueue_message(&session, plain("mine")).unwrap();

    // Act
    let removed = store.remove_queued_message(&other, message.id).unwrap();

    // Assert
    assert!(!removed);
    assert_eq!(store.queued_messages(&session).unwrap().len(), 1);
}

#[test]
fn the_queue_refuses_a_message_past_its_bound() {
    // Arrange
    let (_dir, mut store, session) = open();
    for _ in 0..MAX_QUEUED_MESSAGES {
        store.enqueue_message(&session, plain("x")).unwrap();
    }

    // Act
    let err = store
        .enqueue_message(&session, plain("one too many"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::QueueFull { .. }), "{err}");
}

#[test]
fn the_latest_turn_is_none_before_any_turn() {
    // Arrange
    let (_dir, mut store, session) = open();
    assert_eq!(store.latest_turn(&session).unwrap(), None);

    // Act
    let turn = store.begin_turn(&session).unwrap();

    // Assert
    assert_eq!(store.latest_turn(&session).unwrap(), Some(turn));
}

#[test]
fn queued_messages_survive_reopening_the_store() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    let session = {
        let mut store = Store::open(&data).unwrap();
        let session = new_session(&mut store);
        store.enqueue_message(&session, plain("later")).unwrap();
        session
    };

    // Act
    let store = Store::open(&data).unwrap();

    // Assert
    assert_eq!(store.queued_messages(&session).unwrap()[0].text, "later");
}

#[test]
fn a_message_keeps_the_limits_it_was_queued_with() {
    // Arrange
    let (_dir, mut store, session) = open();
    let limits = NewQueued {
        text: "capped",
        max_turns: Some(3),
        token_cap: Some(500),
    };

    // Act
    store.enqueue_message(&session, limits).unwrap();

    // Assert
    let stored = &store.queued_messages(&session).unwrap()[0];
    assert_eq!((stored.max_turns, stored.token_cap), (Some(3), Some(500)));
}

#[test]
fn text_past_the_bound_is_refused() {
    // Arrange
    let (_dir, mut store, session) = open();
    let text = "x".repeat(MAX_QUEUED_TEXT_BYTES + 1);

    // Act
    let err = store.enqueue_message(&session, plain(&text)).unwrap_err();

    // Assert
    assert!(
        matches!(err, StoreError::QueuedTextTooLarge { .. }),
        "{err}"
    );
}
