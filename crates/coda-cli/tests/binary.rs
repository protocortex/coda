// SPDX-License-Identifier: AGPL-3.0-only

//! The real `coda` binary with `CODA_HOME` pointing at a short temporary directory.

mod common;

use std::process::{Command, Output};

use common::{StopDaemon, short_dir};

const BIN: &str = env!("CARGO_BIN_EXE_coda");

fn coda(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("CODA_HOME", home)
        .output()
        .expect("coda binary should start")
}

fn pid_of(home: &std::path::Path) -> String {
    let status = String::from_utf8(coda(home, &["status"]).stdout).unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("pid: "))
        .unwrap_or_else(|| panic!("no pid in {status:?}"))
        .to_owned()
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

#[test]
fn running_coda_starts_the_daemon_then_ping_and_status_answer() {
    // Arrange
    let home = short_dir();
    let _stop = StopDaemon::new(home.path());

    // Act
    let started = coda(home.path(), &[]);
    let ping = coda(home.path(), &["ping"]);
    let status = coda(home.path(), &["status"]);

    // Assert
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&ping.stdout), "pong\n");
    assert!(String::from_utf8_lossy(&status.stdout).contains("protocol: 1"));
}

#[test]
fn daemon_restart_replaces_the_running_daemon() {
    // Arrange
    let home = short_dir();
    let _stop = StopDaemon::new(home.path());
    assert!(coda(home.path(), &[]).status.success());
    let before = pid_of(home.path());

    // Act
    let restarted = coda(home.path(), &["daemon", "restart"]);

    // Assert
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    assert_ne!(pid_of(home.path()), before);
}
