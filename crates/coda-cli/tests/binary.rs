// SPDX-License-Identifier: AGPL-3.0-only

//! The real `coda` binary with `CODA_HOME` pointing at a short temporary directory.

mod common;

use std::process::{Command, Output};

use common::short_dir;

const BIN: &str = env!("CARGO_BIN_EXE_coda");

fn coda(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("CODA_HOME", home)
        .output()
        .expect("coda binary should start")
}

#[test]
fn status_without_a_daemon_exits_non_zero_with_a_message() {
    // Arrange
    let home = short_dir();

    // Act
    let output = coda(home.path(), &["status"]);

    // Assert
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no daemon is running"));
}
