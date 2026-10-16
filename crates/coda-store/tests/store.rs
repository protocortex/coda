// SPDX-License-Identifier: AGPL-3.0-only

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::thread;

use coda_store::{SCHEMA_VERSION, Store, StoreError, migrate};
use rusqlite::Connection;
use tempfile::TempDir;

const V1_SNAPSHOT: &str = include_str!("snapshots/v1.sql");

fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let store = Store::open(&data_dir).unwrap();
    (dir, store)
}

fn raw(dir: &TempDir) -> Connection {
    let conn = Connection::open(dir.path().join("data/coda.db")).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
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
fn applying_migrations_twice_is_a_no_op() {
    // Arrange
    let mut conn = Connection::open_in_memory().unwrap();
    migrate::migrate(&mut conn).unwrap();
    let before = master_listing(&conn);

    // Act
    migrate::migrate(&mut conn).unwrap();

    // Assert
    assert_eq!(master_listing(&conn), before);
}

#[test]
fn open_enables_wal_mode() {
    // Arrange / Act
    let (dir, _store) = open_store();

    // Assert
    let mode: String = raw(&dir)
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn open_records_the_schema_version() {
    // Arrange / Act
    let (dir, _store) = open_store();

    // Assert
    let version: u32 = raw(&dir)
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(version, 2);
}

#[test]
fn newer_schema_version_is_refused() {
    // Arrange
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "user_version", 99).unwrap();

    // Act
    let err = migrate::migrate(&mut conn).unwrap_err();

    // Assert
    assert!(matches!(
        err,
        StoreError::SchemaTooNew {
            found: 99,
            supported: 2
        }
    ));
}

#[test]
fn data_dir_is_0700_and_database_files_are_0600() {
    // Arrange / Act
    let (dir, _store) = open_store();
    raw(&dir)
        .execute("INSERT INTO sessions (id, created_at) VALUES (1, 0)", [])
        .unwrap();

    // Assert
    let data_dir = dir.path().join("data");
    assert_eq!(mode(&data_dir), 0o700);
    for name in ["coda.db", "coda.db-wal", "coda.db-shm"] {
        assert_eq!(mode(&data_dir.join(name)), 0o600, "{name}");
    }
}

#[test]
fn existing_data_dir_with_loose_mode_is_refused() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o755)).unwrap();

    // Act
    let err = Store::open(&data_dir).unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::InsecureDataDir { .. }));
    assert_eq!(mode(&data_dir), 0o755);
}

#[test]
fn every_required_table_exists() {
    // Arrange
    let (dir, _store) = open_store();
    let required = [
        "sessions",
        "turns",
        "events",
        "usage",
        "event_log",
        "originals",
        "symbols",
        "symbols_fts",
        "pending_permissions",
        "permission_decisions",
        "permission_grants",
    ];

    for table in required {
        // Act
        let count: u32 = raw(&dir)
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();

        // Assert
        assert_eq!(count, 1, "{table}");
    }
}

#[test]
fn foreign_keys_are_enforced() {
    // Arrange
    let (dir, _store) = open_store();

    // Act
    let result = raw(&dir).execute(
        "INSERT INTO turns (id, session_id, started_at) VALUES (1, 404, 0)",
        [],
    );

    // Assert
    assert!(result.is_err());
}

#[test]
fn event_log_rejects_free_text_in_enum_columns() {
    // Arrange
    let (dir, _store) = open_store();

    // Act
    let result = raw(&dir).execute(
        "INSERT INTO event_log (id, kind, at) VALUES (1, 'my secret prompt text', 0)",
        [],
    );

    // Assert
    assert!(result.is_err());
}

#[test]
fn v1_snapshot_migrates_forward_to_the_current_schema() {
    // Arrange
    let mut old = Connection::open_in_memory().unwrap();
    old.execute_batch(V1_SNAPSHOT).unwrap();
    old.pragma_update(None, "user_version", 1).unwrap();
    let mut fresh = Connection::open_in_memory().unwrap();
    migrate::migrate(&mut fresh).unwrap();

    // Act
    migrate::migrate(&mut old).unwrap();

    // Assert
    let version: u32 = old
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(master_listing(&old), master_listing(&fresh));
}

fn seed_turn(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO sessions (id, created_at) VALUES (1, 0);
         INSERT INTO turns (id, session_id, started_at) VALUES (1, 1, 0);",
    )
    .unwrap();
}

#[test]
fn events_reject_a_duplicate_sequence_number_within_a_turn() {
    // Arrange
    let (dir, _store) = open_store();
    let conn = raw(&dir);
    seed_turn(&conn);
    let insert = "INSERT INTO events (turn_id, seq, kind, body, at) VALUES (1, 1, 'k', 'b', 0)";
    conn.execute(insert, []).unwrap();

    // Act
    let result = conn.execute(insert, []);

    // Assert
    assert!(result.is_err());
}

#[test]
fn every_foreign_key_column_is_indexed() {
    // Arrange
    let (dir, _store) = open_store();
    let conn = raw(&dir);
    let tables = [
        ("turns", "session_id"),
        ("events", "turn_id"),
        ("usage", "turn_id"),
        ("originals", "session_id"),
        ("pending_permissions", "session_id"),
        ("permission_decisions", "session_id"),
    ];

    for (table, column) in tables {
        // Act
        let indexed: u32 = conn
            .query_row(
                "SELECT count(*) FROM pragma_index_list(?1) l \
                 JOIN pragma_index_info(l.name) i \
                 WHERE i.seqno = 0 AND i.name = ?2",
                [table, column],
                |r| r.get(0),
            )
            .unwrap();

        // Assert
        assert!(indexed >= 1, "{table}.{column}");
    }
}

fn fts_hits(conn: &Connection, term: &str) -> u32 {
    conn.query_row(
        "SELECT count(*) FROM symbols_fts WHERE symbols_fts MATCH ?1",
        [term],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn symbol_search_index_follows_inserts_updates_and_deletes() {
    // Arrange
    let (dir, _store) = open_store();
    let conn = raw(&dir);

    // Act
    conn.execute(
        "INSERT INTO symbols (id, repo_path, file_path, name, kind, line) \
         VALUES (1, 'r', 'f', 'frobnicate', 'fn', 3)",
        [],
    )
    .unwrap();

    // Assert
    assert_eq!(fts_hits(&conn, "frobnicate"), 1);

    // Act
    conn.execute("UPDATE symbols SET name = 'wibble' WHERE id = 1", [])
        .unwrap();

    // Assert
    assert_eq!(fts_hits(&conn, "frobnicate"), 0);
    assert_eq!(fts_hits(&conn, "wibble"), 1);

    // Act
    conn.execute("DELETE FROM symbols WHERE id = 1", [])
        .unwrap();

    // Assert
    assert_eq!(fts_hits(&conn, "wibble"), 0);
}

#[test]
fn concurrent_first_opens_both_succeed() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let data_dir = data_dir.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                Store::open(&data_dir).map(|_| ())
            })
        })
        .collect();

    // Act
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // Assert
    for result in results {
        result.unwrap();
    }
}

#[test]
fn too_new_database_is_refused_before_the_journal_mode_changes() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let db_path = data_dir.join("coda.db");
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600)).unwrap();
    let header_before = fs::read(&db_path).unwrap()[18..20].to_vec();

    // Act
    let err = Store::open(&data_dir).unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::SchemaTooNew { found: 99, .. }));
    assert_eq!(fs::read(&db_path).unwrap()[18..20], header_before[..]);
    assert_eq!(header_before, [1, 1], "rollback-journal format bytes");
}

#[test]
fn existing_database_file_with_loose_mode_is_refused() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let db_path = data_dir.join("coda.db");
    fs::write(&db_path, b"").unwrap();
    fs::set_permissions(&db_path, fs::Permissions::from_mode(0o644)).unwrap();

    // Act
    let err = Store::open(&data_dir).unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::InsecureDbFile { .. }));
    assert_eq!(mode(&db_path), 0o644);
}

#[test]
fn symlinked_data_dir_is_refused() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let real = dir.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // Act
    let err = Store::open(&link).unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::SymlinkedDataDir { .. }));
    assert!(!real.join("coda.db").exists());
}
