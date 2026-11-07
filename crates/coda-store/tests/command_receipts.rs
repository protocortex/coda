// SPDX-License-Identifier: Apache-2.0

//! The answers kept for commands that carried a client request id.

use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::{NewSession, RECEIPT_RETENTION_MS, Store};
use tempfile::TempDir;

const REQUEST: &str = "req-1";
const SEND: &str = "session.send";

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
fn an_unknown_request_has_no_receipt() {
    // Arrange
    let (_dir, store, _session) = open();

    // Act
    let found = store.receipt(REQUEST).unwrap();

    // Assert
    assert_eq!(found, None);
}

#[test]
fn a_recorded_receipt_is_read_back_with_its_method_session_and_answer() {
    // Arrange
    let (_dir, mut store, session) = open();

    // Act
    let recorded = store
        .record_receipt(REQUEST, SEND, Some(&session), r#"{"turn_id":"7"}"#)
        .unwrap();

    // Assert
    let found = store.receipt(REQUEST).unwrap().unwrap();
    assert!(recorded);
    assert_eq!(found.method, SEND);
    assert_eq!(found.session, Some(session));
    assert_eq!(found.result_json, r#"{"turn_id":"7"}"#);
}

#[test]
fn a_second_record_for_the_same_id_keeps_the_first_answer() {
    // Arrange
    let (_dir, mut store, session) = open();
    store
        .record_receipt(REQUEST, SEND, Some(&session), "first")
        .unwrap();

    // Act
    let recorded = store
        .record_receipt(REQUEST, SEND, Some(&session), "second")
        .unwrap();

    // Assert
    assert!(!recorded);
    assert_eq!(
        store.receipt(REQUEST).unwrap().unwrap().result_json,
        "first"
    );
}

#[test]
fn a_turn_start_with_a_request_id_writes_its_receipt_with_the_turn() {
    // Arrange
    let (_dir, mut store, session) = open();

    // Act
    let turn = store
        .begin_turn_for_request(&session, REQUEST, SEND)
        .unwrap();

    // Assert
    let found = store.receipt(REQUEST).unwrap().unwrap();
    assert_eq!(found.result_json, format!(r#"{{"turn_id":"{turn}"}}"#));
    assert_eq!(found.session, Some(session));
}

#[test]
fn a_turn_start_that_fails_leaves_no_receipt() {
    // Arrange
    let (_dir, mut store, _session) = open();
    let missing = SessionId::new("999");

    // Act
    let started = store.begin_turn_for_request(&missing, REQUEST, SEND);

    // Assert
    assert!(started.is_err());
    assert_eq!(store.receipt(REQUEST).unwrap(), None);
}

#[test]
fn a_forgotten_receipt_lets_the_request_start_again() {
    // Arrange
    let (_dir, mut store, session) = open();
    let first = store
        .begin_turn_for_request(&session, REQUEST, SEND)
        .unwrap();
    store.end_turn(&session, &first).unwrap();
    store.forget_receipt(REQUEST).unwrap();

    // Act
    let again = store.begin_turn_for_request(&session, REQUEST, SEND);

    // Assert
    assert!(again.is_ok(), "{again:?}");
}

#[test]
fn a_repeated_turn_start_does_not_begin_a_second_turn() {
    // Arrange
    let (_dir, mut store, session) = open();
    store
        .begin_turn_for_request(&session, REQUEST, SEND)
        .unwrap();

    // Act
    let again = store.begin_turn_for_request(&session, REQUEST, SEND);

    // Assert
    assert!(again.is_err());
    assert_eq!(store.unfinished_turns().unwrap().len(), 1);
}

#[test]
fn a_session_created_with_a_request_id_writes_its_receipt_with_the_session() {
    // Arrange
    let (_dir, mut store, _session) = open();
    let new = NewSession {
        backend: BackendKind::Faux,
        model: None,
        cwd: "/other".to_owned(),
        mode: Mode::Default,
    };

    // Act
    let created = store
        .create_session_for_request(&new, REQUEST, "session.create", |record| {
            format!("created {}", record.id)
        })
        .unwrap();

    // Assert
    let found = store.receipt(REQUEST).unwrap().unwrap();
    assert_eq!(found.result_json, format!("created {}", created.id));
}

#[test]
fn old_receipts_are_dropped_when_a_new_one_is_written() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let clock = std::sync::Arc::new(ManualClock::default());
    let mut store = Store::open_with_clock(&dir.path().join("data"), clock.clone()).unwrap();
    let session = new_session(&mut store);
    store
        .record_receipt("old", SEND, Some(&session), "x")
        .unwrap();
    clock.advance(RECEIPT_RETENTION_MS + 1);

    // Act
    store
        .record_receipt("new", SEND, Some(&session), "y")
        .unwrap();

    // Assert
    assert_eq!(store.receipt("old").unwrap(), None);
    assert!(store.receipt("new").unwrap().is_some());
}

#[derive(Debug, Default)]
struct ManualClock(std::sync::atomic::AtomicI64);

impl ManualClock {
    fn advance(&self, ms: i64) {
        self.0.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl coda_store::Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}
