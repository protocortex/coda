// SPDX-License-Identifier: AGPL-3.0-only

//! The per repository decision to trust project agent definitions.

use coda_store::Store;
use tempfile::TempDir;

const REPO: &str = "/work/repo";
const OTHER_REPO: &str = "/work/other";

fn open() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("data")).unwrap();
    (dir, store)
}

#[test]
fn a_repo_has_no_digest_until_a_decision_is_recorded() {
    // Arrange
    let (_dir, store) = open();

    // Act
    let digest = store.project_trust_digest(REPO).unwrap();

    // Assert
    assert_eq!(digest, None);
}

#[test]
fn trust_is_recorded_per_repo_and_trusting_again_replaces_the_digest() {
    // Arrange
    let (_dir, mut store) = open();

    // Act
    store.trust_project(REPO, "d1").unwrap();
    store.trust_project(REPO, "d2").unwrap();

    // Assert
    assert_eq!(
        store.project_trust_digest(REPO).unwrap().as_deref(),
        Some("d2")
    );
    assert_eq!(store.project_trust_digest(OTHER_REPO).unwrap(), None);
}

#[test]
fn revoking_removes_trust_and_reports_whether_it_existed() {
    // Arrange
    let (_dir, mut store) = open();
    store.trust_project(REPO, "d1").unwrap();

    // Act
    let first = store.revoke_project_trust(REPO).unwrap();
    let second = store.revoke_project_trust(REPO).unwrap();

    // Assert
    assert!(first);
    assert!(!second);
    assert_eq!(store.project_trust_digest(REPO).unwrap(), None);
}
