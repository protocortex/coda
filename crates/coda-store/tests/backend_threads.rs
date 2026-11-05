// SPDX-License-Identifier: Apache-2.0

//! The backend's own conversation id per session, kept across daemon restarts.

use coda_protocol::types::{BackendKind, Mode, SessionId, TurnId};
use coda_store::{NewSession, SCHEMA_VERSION, Store, migrate};
use rusqlite::Connection;
use tempfile::TempDir;

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

fn turn(n: u32) -> TurnId {
    TurnId::new(n.to_string())
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
        .record_backend_thread(&session, BACKEND, NATIVE_REF, &turn(1))
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
fn recording_the_same_ref_again_moves_only_the_last_turn() {
    // Arrange
    let (dir, mut store, session) = open();
    store
        .record_backend_thread(&session, BACKEND, NATIVE_REF, &turn(1))
        .unwrap();

    // Act
    store
        .record_backend_thread(&session, BACKEND, NATIVE_REF, &turn(2))
        .unwrap();

    // Assert
    let conn = Connection::open(dir.path().join("data/coda.db")).unwrap();
    let row: (String, String, String) = conn
        .query_row(
            "SELECT native_ref, first_turn, last_turn FROM backend_threads",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(row, (NATIVE_REF.to_owned(), "1".to_owned(), "2".to_owned()));
}

#[test]
fn a_new_ref_replaces_the_old_one_and_starts_a_new_first_turn() {
    // Arrange
    let (dir, mut store, session) = open();
    store
        .record_backend_thread(&session, BACKEND, NATIVE_REF, &turn(1))
        .unwrap();
    store.fail_backend_thread(&session, BACKEND).unwrap();

    // Act
    store
        .record_backend_thread(&session, BACKEND, "native-2", &turn(3))
        .unwrap();

    // Assert
    assert_eq!(
        store
            .active_backend_ref(&session, BACKEND)
            .unwrap()
            .as_deref(),
        Some("native-2")
    );
    let conn = Connection::open(dir.path().join("data/coda.db")).unwrap();
    let first: String = conn
        .query_row("SELECT first_turn FROM backend_threads", [], |r| r.get(0))
        .unwrap();
    assert_eq!(first, "3");
}

#[test]
fn a_failed_thread_is_not_offered_for_resume() {
    // Arrange
    let (_dir, mut store, session) = open();
    store
        .record_backend_thread(&session, BACKEND, NATIVE_REF, &turn(1))
        .unwrap();

    // Act
    store.fail_backend_thread(&session, BACKEND).unwrap();

    // Assert
    assert_eq!(store.active_backend_ref(&session, BACKEND).unwrap(), None);
}

#[test]
fn refs_are_kept_per_backend() {
    // Arrange
    let (_dir, mut store, session) = open();
    store
        .record_backend_thread(&session, BACKEND, NATIVE_REF, &turn(1))
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
