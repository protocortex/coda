// SPDX-License-Identifier: AGPL-3.0-only

//! Incremental index state: file hashes, atomic replacement, ranked search, status.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use coda_protocol::readers::IndexReader;
use coda_store::{Clock, Store, Symbol};
use rusqlite::Connection;
use tempfile::TempDir;

const REPO: &str = "/repo";
/// Most rows a search returns, whatever limit the caller asks for.
const SEARCH_CAP: usize = 100;

#[derive(Debug, Default)]
struct FixedClock(AtomicI64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn open() -> (TempDir, Store, Arc<FixedClock>) {
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(FixedClock::default());
    let store = Store::open_with_clock(&dir.path().join("data"), clock.clone()).unwrap();
    (dir, store, clock)
}

fn raw(dir: &TempDir) -> Connection {
    Connection::open(dir.path().join("data/coda.db")).unwrap()
}

fn symbol(name: &str, line: u32) -> Symbol {
    Symbol {
        name: name.to_owned(),
        kind: "fn".to_owned(),
        line,
        end_line: line + 2,
        parent: None,
    }
}

fn hits(store: &Store, repo: &str, query: &str, limit: usize) -> Vec<(String, u32)> {
    let reader = store.index_reader(repo);
    let view: &dyn IndexReader = &reader;
    view.search(query, limit)
        .unwrap()
        .into_iter()
        .map(|h| (h.path, h.line))
        .collect()
}

#[test]
fn replace_file_records_hash_and_time_per_file() {
    // Arrange
    let (_dir, mut store, clock) = open();
    clock.0.store(1_700, Ordering::SeqCst);

    // Act
    store
        .replace_file(REPO, "src/a.rs", "hash-a", vec![symbol("alpha", 1)])
        .unwrap();
    let files = store.indexed_files(REPO).unwrap();

    // Assert
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].file_path, "src/a.rs");
    assert_eq!(files[0].content_hash, "hash-a");
    assert_eq!(files[0].indexed_at, 1_700);
}

#[test]
fn restamp_moves_the_index_time_and_keeps_hash_and_symbols() {
    // Arrange
    let (_dir, mut store, clock) = open();
    clock.0.store(100, Ordering::SeqCst);
    store
        .replace_file(REPO, "a.rs", "h", vec![symbol("alpha", 1)])
        .unwrap();
    clock.0.store(900, Ordering::SeqCst);

    // Act
    store.restamp_files(REPO, &["a.rs".to_owned()]).unwrap();

    // Assert
    let files = store.indexed_files(REPO).unwrap();
    assert_eq!(
        (files[0].content_hash.as_str(), files[0].indexed_at),
        ("h", 900)
    );
    assert_eq!(hits(&store, REPO, "alpha", 10).len(), 1);
}

#[test]
fn replace_file_swaps_old_rows_for_new_ones() {
    // Arrange
    let (_dir, mut store, _clock) = open();
    store
        .replace_file(REPO, "src/a.rs", "h1", vec![symbol("old_name", 1)])
        .unwrap();

    // Act
    store
        .replace_file(REPO, "src/a.rs", "h2", vec![symbol("new_name", 4)])
        .unwrap();

    // Assert
    assert_eq!(hits(&store, REPO, "old_name", 10).len(), 0);
    assert_eq!(
        hits(&store, REPO, "new_name", 10),
        [("src/a.rs".to_owned(), 4)]
    );
    assert_eq!(store.indexed_files(REPO).unwrap()[0].content_hash, "h2");
}

#[test]
fn failed_replace_keeps_the_previous_rows_and_hash() {
    // Arrange
    let (dir, mut store, _clock) = open();
    store
        .replace_file(REPO, "src/a.rs", "h1", vec![symbol("kept", 1)])
        .unwrap();
    raw(&dir)
        .execute_batch(
            "CREATE TRIGGER poison BEFORE INSERT ON symbols WHEN new.name = 'poison' \
             BEGIN SELECT RAISE(ABORT, 'rejected'); END;",
        )
        .unwrap();

    // Act
    let result = store.replace_file(
        REPO,
        "src/a.rs",
        "h2",
        vec![symbol("fresh", 1), symbol("poison", 2)],
    );

    // Assert
    assert!(result.is_err());
    assert_eq!(hits(&store, REPO, "kept", 10).len(), 1);
    assert_eq!(hits(&store, REPO, "fresh", 10).len(), 0);
    assert_eq!(store.indexed_files(REPO).unwrap()[0].content_hash, "h1");
}

#[test]
fn clear_file_drops_its_symbols_and_hash() {
    // Arrange
    let (_dir, mut store, _clock) = open();
    store
        .replace_file(REPO, "src/a.rs", "h1", vec![symbol("gone", 1)])
        .unwrap();

    // Act
    store.clear_file(REPO, "src/a.rs").unwrap();

    // Assert
    assert_eq!(hits(&store, REPO, "gone", 10).len(), 0);
    assert_eq!(store.indexed_files(REPO).unwrap().len(), 0);
}

#[test]
fn ties_order_by_path_then_line() {
    // Arrange
    let (_dir, mut store, _clock) = open();
    store
        .replace_file(REPO, "src/b.rs", "hb", vec![symbol("same", 3)])
        .unwrap();
    store
        .replace_file(
            REPO,
            "src/a.rs",
            "ha",
            vec![symbol("same", 9), symbol("same", 2)],
        )
        .unwrap();

    // Act
    let found = hits(&store, REPO, "same", 10);

    // Assert
    let expected =
        [("src/a.rs", 2), ("src/a.rs", 9), ("src/b.rs", 3)].map(|(p, l)| (p.to_owned(), l));
    assert_eq!(found, expected);
}

#[test]
fn a_shorter_name_outranks_longer_ones_regardless_of_path() {
    // Arrange
    let (_dir, mut store, _clock) = open();
    store
        .replace_file(
            REPO,
            "src/a.rs",
            "ha",
            vec![symbol("parse extra words here", 1)],
        )
        .unwrap();
    store
        .replace_file(REPO, "src/b.rs", "hb", vec![symbol("parse extra", 4)])
        .unwrap();
    store
        .replace_file(REPO, "src/z.rs", "hz", vec![symbol("parse", 9)])
        .unwrap();

    // Act
    let found = hits(&store, REPO, "parse", 10);

    // Assert
    let expected =
        [("src/z.rs", 9), ("src/b.rs", 4), ("src/a.rs", 1)].map(|(p, l)| (p.to_owned(), l));
    assert_eq!(found, expected);
}

#[test]
fn hostile_query_text_is_matched_as_plain_text() {
    // Arrange
    let (_dir, mut store, _clock) = open();
    store
        .replace_file(REPO, "src/a.rs", "ha", vec![symbol("alpha", 1)])
        .unwrap();
    let queries = [
        "\"",
        "\"alpha",
        "alpha\"\"",
        "alpha AND",
        "OR beta",
        "NOT alpha",
        "NEAR(alpha beta, 2)",
        "alpha NEAR beta",
        "name:alpha",
        "*",
        "(((",
        "alpha) OR (",
        "-alpha",
        "^alpha",
        "",
        "   ",
        "'; DROP TABLE symbols; --",
    ];

    // Act
    let outcomes: Vec<_> = queries
        .iter()
        .map(|q| {
            let reader = store.index_reader(REPO);
            let view: &dyn IndexReader = &reader;
            view.search(q, 10)
        })
        .collect();

    // Assert
    assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");
    assert_eq!(hits(&store, REPO, "alpha", 10).len(), 1);
}

#[test]
fn result_limit_is_capped() {
    // Arrange
    let (_dir, mut store, _clock) = open();
    let many = (1..=150).map(|n| symbol("dup", n)).collect();
    store.replace_file(REPO, "src/a.rs", "ha", many).unwrap();

    // Act
    let found = hits(&store, REPO, "dup", usize::MAX);

    // Assert
    assert_eq!(found.len(), SEARCH_CAP);
}

#[test]
fn repos_do_not_share_files_or_hits() {
    // Arrange
    let (_dir, mut store, _clock) = open();
    store
        .replace_file("/one", "src/a.rs", "h1", vec![symbol("shared", 1)])
        .unwrap();
    store
        .replace_file("/two", "src/a.rs", "h2", vec![symbol("shared", 5)])
        .unwrap();

    // Act
    store.clear_file("/one", "src/a.rs").unwrap();

    // Assert
    assert_eq!(store.indexed_files("/one").unwrap().len(), 0);
    assert_eq!(store.indexed_files("/two").unwrap()[0].content_hash, "h2");
    assert_eq!(
        hits(&store, "/two", "shared", 10),
        [("src/a.rs".to_owned(), 5)]
    );
}

#[test]
fn status_reports_counts_and_the_latest_index_time() {
    // Arrange
    let (_dir, mut store, clock) = open();
    assert_eq!(store.index_status(REPO).unwrap().last_indexed_at, None);
    clock.0.store(100, Ordering::SeqCst);
    store
        .replace_file(
            REPO,
            "src/a.rs",
            "ha",
            vec![symbol("a1", 1), symbol("a2", 2)],
        )
        .unwrap();
    clock.0.store(250, Ordering::SeqCst);
    store
        .replace_file(REPO, "src/b.rs", "hb", vec![symbol("b1", 1)])
        .unwrap();

    // Act
    let status = store.index_status(REPO).unwrap();

    // Assert
    assert_eq!(status.files, 2);
    assert_eq!(status.symbols, 3);
    assert_eq!(status.last_indexed_at, Some(250));
}
