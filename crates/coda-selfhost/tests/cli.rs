// SPDX-License-Identifier: AGPL-3.0-only

//! The `coda-selfhost` binary the shell scripts call.

use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_coda-selfhost");

#[test]
fn verify_exits_one_for_a_copy_with_no_changes() {
    // Arrange
    let repo = tempfile::tempdir().unwrap();
    let setup = "git init -q && git -c user.name=t -c user.email=t@t -c commit.gpgsign=false \
                 commit -q --allow-empty -m base";
    assert!(
        Command::new("sh")
            .args(["-c", setup])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );

    // Act
    let output = Command::new(BIN)
        .arg("verify")
        .arg("--workdir")
        .arg(repo.path())
        .output()
        .unwrap();

    // Assert
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("changed nothing"));
}

#[test]
fn a_missing_workdir_exits_three_and_a_usage_error_prints_usage() {
    // Act
    let missing = Command::new(BIN)
        .args(["verify", "--workdir", "/no/such/dir/for/selfhost"])
        .output()
        .unwrap();
    let usage = Command::new(BIN).arg("verify").output().unwrap();

    // Assert
    assert_eq!(missing.status.code(), Some(3));
    assert_eq!(usage.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&usage.stderr).contains("Usage"));
}
