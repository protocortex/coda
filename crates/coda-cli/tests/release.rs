// SPDX-License-Identifier: AGPL-3.0-only

//! The first release: the version the binary reports and the changelog entry for it.

use std::path::Path;
use std::process::Command;

#[test]
fn the_binary_version_matches_the_store_release() {
    // Arrange
    let expected = format!("coda {}\n", coda_store::BINARY_RELEASE);

    // Act
    let output = Command::new(env!("CARGO_BIN_EXE_coda"))
        .arg("--version")
        .output()
        .expect("coda binary should start");

    // Assert
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

#[test]
fn the_changelog_has_an_entry_for_the_current_version() {
    // Arrange
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../CHANGELOG.md");
    let heading = format!("## [{}]", env!("CARGO_PKG_VERSION"));

    // Act
    let changelog = std::fs::read_to_string(&path).unwrap_or_default();

    // Assert
    assert!(
        changelog.lines().any(|line| line.starts_with(&heading)),
        "CHANGELOG.md has no `{heading}` heading"
    );
}
