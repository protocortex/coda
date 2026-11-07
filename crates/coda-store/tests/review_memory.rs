// SPDX-License-Identifier: Apache-2.0

//! What `coda review` remembers about a pull request between runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use coda_store::{
    Clock, FindingStatus, MemoryFinding, SCHEMA_VERSION, Store, StoredFinding, migrate,
};
use rusqlite::Connection;
use tempfile::TempDir;

const V12_SNAPSHOT: &str = include_str!("snapshots/v12.sql");
const PR: &str = "acme/widgets#7";

#[derive(Debug)]
struct Ticking(AtomicI64);

impl Clock for Ticking {
    fn now_ms(&self) -> i64 {
        self.0.fetch_add(1000, Ordering::Relaxed)
    }
}

fn open() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open_with_clock(
        &dir.path().join("data"),
        Arc::new(Ticking(AtomicI64::new(1000))),
    )
    .unwrap();
    (dir, store)
}

fn finding(fingerprint: &str, code_hash: &str) -> MemoryFinding {
    MemoryFinding {
        fingerprint: fingerprint.to_owned(),
        file: "a.rs".to_owned(),
        start_line: 3,
        end_line: 4,
        severity: "high".to_owned(),
        title: format!("title {fingerprint}"),
        explanation: "why".to_owned(),
        code_hash: code_hash.to_owned(),
    }
}

fn status_of(found: &[StoredFinding], fingerprint: &str) -> FindingStatus {
    found
        .iter()
        .find(|f| f.finding.fingerprint == fingerprint)
        .unwrap()
        .status
}

#[test]
fn a_database_from_before_review_memory_migrates_forward() {
    // Arrange
    let mut old = Connection::open_in_memory().unwrap();
    old.execute_batch(V12_SNAPSHOT).unwrap();
    old.pragma_update(None, "user_version", 12).unwrap();

    // Act
    migrate::migrate(&mut old).unwrap();

    // Assert
    let version: u32 = old
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    for table in ["review_runs", "review_findings"] {
        let n: i64 = old
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "{table}");
    }
}

#[test]
fn a_pull_request_never_reviewed_has_no_run_and_no_findings() {
    // Arrange
    let (_dir, store) = open();

    // Act and Assert
    assert_eq!(store.last_review_run(PR).unwrap(), None);
    assert_eq!(store.review_findings(PR).unwrap(), vec![]);
}

#[test]
fn a_run_records_its_head_and_its_findings_as_open() {
    // Arrange
    let (_dir, mut store) = open();

    // Act
    store
        .record_review_run(PR, "sha1", true, &[finding("f1", "h1")], &[])
        .unwrap();

    // Assert
    let run = store.last_review_run(PR).unwrap().unwrap();
    assert_eq!((run.head_sha.as_str(), run.full), ("sha1", true));
    let found = store.review_findings(PR).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(status_of(&found, "f1"), FindingStatus::Open);
    assert_eq!(found[0].finding.code_hash, "h1");
    assert_eq!(store.review_findings("other#1").unwrap(), vec![]);
}

#[test]
fn a_finding_reported_again_stays_one_row_with_the_newest_text_and_hash() {
    // Arrange
    let (_dir, mut store) = open();
    store
        .record_review_run(PR, "sha1", true, &[finding("f1", "h1")], &[])
        .unwrap();
    let mut again = finding("f1", "h2");
    again.explanation = "reworded".to_owned();

    // Act
    store
        .record_review_run(PR, "sha2", true, &[again], &[])
        .unwrap();

    // Assert
    let found = store.review_findings(PR).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].finding.code_hash, "h2");
    assert_eq!(found[0].finding.explanation, "reworded");
}

#[test]
fn a_dismissed_finding_stays_dismissed_when_the_model_reports_it_again() {
    // Arrange
    let (_dir, mut store) = open();
    store
        .record_review_run(PR, "sha1", true, &[finding("f1", "h1")], &[])
        .unwrap();
    assert_eq!(store.dismiss_finding(PR, "f1").unwrap(), 1);

    // Act
    store
        .record_review_run(PR, "sha2", true, &[finding("f1", "h1")], &[])
        .unwrap();

    // Assert
    let found = store.review_findings(PR).unwrap();
    assert_eq!(status_of(&found, "f1"), FindingStatus::Dismissed);
}

#[test]
fn a_full_run_resolves_the_findings_it_names_and_a_later_report_reopens_them() {
    // Arrange
    let (_dir, mut store) = open();
    store
        .record_review_run(
            PR,
            "sha1",
            true,
            &[finding("f1", "h1"), finding("f2", "h2")],
            &[],
        )
        .unwrap();
    store.dismiss_finding(PR, "f2").unwrap();

    // Act
    store
        .record_review_run(PR, "sha2", true, &[], &["f1".to_owned(), "f2".to_owned()])
        .unwrap();
    let after_resolve = store.review_findings(PR).unwrap();
    store
        .record_review_run(PR, "sha3", true, &[finding("f1", "h3")], &[])
        .unwrap();
    let after_report = store.review_findings(PR).unwrap();

    // Assert
    assert_eq!(status_of(&after_resolve, "f1"), FindingStatus::Resolved);
    assert_eq!(status_of(&after_resolve, "f2"), FindingStatus::Dismissed);
    assert_eq!(status_of(&after_report, "f1"), FindingStatus::Open);
}

#[test]
fn an_empty_prefix_dismisses_nothing() {
    // Arrange
    let (_dir, mut store) = open();
    store
        .record_review_run(PR, "sha1", true, &[finding("f1", "h")], &[])
        .unwrap();

    // Act
    let matched = store.dismiss_finding(PR, "").unwrap();

    // Assert
    assert_eq!(matched, 0);
    assert_eq!(
        status_of(&store.review_findings(PR).unwrap(), "f1"),
        FindingStatus::Open
    );
}

#[test]
fn dismissing_takes_a_unique_prefix_and_reports_how_many_matched() {
    // Arrange
    let (_dir, mut store) = open();
    store
        .record_review_run(
            PR,
            "sha1",
            true,
            &[
                finding("ab12", "h"),
                finding("ab34", "h"),
                finding("cd56", "h"),
            ],
            &[],
        )
        .unwrap();

    // Act
    let none = store.dismiss_finding(PR, "zz").unwrap();
    let two = store.dismiss_finding(PR, "ab").unwrap();
    let one = store.dismiss_finding(PR, "cd").unwrap();

    // Assert
    assert_eq!((none, two, one), (0, 2, 1));
    let found = store.review_findings(PR).unwrap();
    assert_eq!(status_of(&found, "ab12"), FindingStatus::Open);
    assert_eq!(status_of(&found, "cd56"), FindingStatus::Dismissed);
}
