// SPDX-License-Identifier: Apache-2.0

//! The backend's own conversation id per session, kept across daemon restarts.

use coda_protocol::types::{BackendKind, Mode, SessionId};
use coda_store::{NewSession, SCHEMA_VERSION, Store, migrate};
use rusqlite::Connection;
use tempfile::TempDir;

const V11_SNAPSHOT: &str = include_str!("snapshots/v11.sql");
const BEFORE_TURN_COLUMNS_DROPPED: u32 = 11;
const V10_SNAPSHOT: &str = include_str!("snapshots/v10.sql");
const BEFORE_BACKEND_THREADS: u32 = 10;
const BACKEND: BackendKind = BackendKind::ClaudeCode;
const NATIVE_REF: &str = "native-1";

fn open() -> (TempDir, Store, SessionId) {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let session = store
        .create_session(&NewSession {
            backend: BACKEND,
            model: None,
            cwd: "/work".to_owned(),
            mode: Mode::Default,
        })
        .unwrap();
    (dir, store, session)
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

#[test]
fn a_session_has_no_resume_ref_until_one_is_recorded() {
    // Arrange
    let (_dir, store, session) = open();

    // Act
    let found = store.active_backend_ref(&session, BACKEND).unwrap();

    // Assert
    assert_eq!(found, None);
}

#[test]
fn a_recorded_ref_is_read_back_after_the_store_is_reopened() {
    // Arrange
    let (dir, mut store, session) = open();
    store
        .record_backend_thread(&session, BACKEND, NATIVE_REF)
        .unwrap();
    drop(store);

    // Act
    let reopened = Store::open(&dir.path().join("data")).unwrap();

    // Assert
    assert_eq!(
        reopened
            .active_backend_ref(&session, BACKEND)
            .unwrap()
            .as_deref(),
        Some(NATIVE_REF)
    );
}

#[test]
fn a_forgotten_ref_is_no_longer_resumed_and_a_later_one_is_kept() {
    // Arrange
    let (_dir, mut store, session) = open();
    store
        .record_backend_thread(&session, BACKEND, NATIVE_REF)
        .unwrap();

    // Act
    store.forget_backend_thread(&session, BACKEND).unwrap();

    // Assert
    assert_eq!(store.active_backend_ref(&session, BACKEND).unwrap(), None);
    store
        .record_backend_thread(&session, BACKEND, "native-2")
        .unwrap();
    assert_eq!(
        store
            .active_backend_ref(&session, BACKEND)
            .unwrap()
            .as_deref(),
        Some("native-2")
    );
}

#[test]
fn forgetting_a_ref_that_was_never_recorded_is_not_an_error() {
    // Arrange
    let (_dir, mut store, session) = open();

    // Act
    let result = store.forget_backend_thread(&session, BACKEND);

    // Assert
    assert!(result.is_ok());
}

#[test]
fn a_new_ref_replaces_the_old_one() {
    // Arrange
    let (_dir, mut store, session) = open();
    store
        .record_backend_thread(&session, BACKEND, NATIVE_REF)
        .unwrap();

    // Act
    store
        .record_backend_thread(&session, BACKEND, "native-2")
        .unwrap();

    // Assert
    assert_eq!(
        store
            .active_backend_ref(&session, BACKEND)
            .unwrap()
            .as_deref(),
        Some("native-2")
    );
}

#[test]
fn refs_are_kept_per_backend() {
    // Arrange
    let (_dir, mut store, session) = open();
    store
        .record_backend_thread(&session, BACKEND, NATIVE_REF)
        .unwrap();

    // Act
    let other = store
        .active_backend_ref(&session, BackendKind::CodexAppServer)
        .unwrap();

    // Assert
    assert_eq!(other, None);
}

#[test]
fn v10_snapshot_migrates_forward_adding_backend_threads() {
    // Arrange
    let mut old = Connection::open_in_memory().unwrap();
    old.execute_batch(V10_SNAPSHOT).unwrap();
    old.pragma_update(None, "user_version", BEFORE_BACKEND_THREADS)
        .unwrap();
    old.execute_batch("INSERT INTO sessions (id, created_at) VALUES (1, 0);")
        .unwrap();
    let mut fresh = Connection::open_in_memory().unwrap();
    migrate::migrate(&mut fresh).unwrap();

    // Act
    migrate::migrate(&mut old).unwrap();

    // Assert
    let version: u32 = old
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let rows: i64 = old
        .query_row("SELECT count(*) FROM backend_threads", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
    let sessions: i64 = old
        .query_row("SELECT count(*) FROM sessions WHERE id = 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(sessions, 1, "the existing session survives the migration");
    assert_eq!(master_listing(&old), master_listing(&fresh));
}

#[test]
fn v11_snapshot_migrates_forward_dropping_the_unused_turn_columns() {
    // Arrange
    let mut old = Connection::open_in_memory().unwrap();
    old.execute_batch(V11_SNAPSHOT).unwrap();
    old.pragma_update(None, "user_version", BEFORE_TURN_COLUMNS_DROPPED)
        .unwrap();
    old.execute_batch(
        "INSERT INTO sessions (id, created_at) VALUES (1, 0); \
         INSERT INTO backend_threads (session_id, backend, native_ref, first_turn, last_turn, status) \
         VALUES (1, 'claude_code', 'native-1', '1', '2', 'active');",
    )
    .unwrap();
    let mut fresh = Connection::open_in_memory().unwrap();
    migrate::migrate(&mut fresh).unwrap();

    // Act
    migrate::migrate(&mut old).unwrap();

    // Assert
    let version: u32 = old
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let (native_ref, status): (String, String) = old
        .query_row(
            "SELECT native_ref, status FROM backend_threads WHERE session_id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (native_ref.as_str(), status.as_str()),
        ("native-1", "active")
    );
    assert!(
        old.prepare("SELECT first_turn FROM backend_threads")
            .is_err(),
        "first_turn is gone"
    );
    assert!(
        old.prepare("SELECT last_turn FROM backend_threads")
            .is_err(),
        "last_turn is gone"
    );
    assert_eq!(master_listing(&old), master_listing(&fresh));
}
