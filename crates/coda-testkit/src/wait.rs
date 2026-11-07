// SPDX-License-Identifier: Apache-2.0

//! Bounded waits for tests that run real subprocesses.

use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

const KILL_PROGRAM: &str = "kill";
const KILL_SIGNAL: &str = "-KILL";

/// Waits for `child` and returns its output, or kills it and returns `None`
/// once `limit` passes. It blocks on the child, so it never sleeps in a loop.
#[must_use]
pub fn output_within(child: Child, limit: Duration) -> Option<Output> {
    let pid = child.id().to_string();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    match receiver.recv_timeout(limit) {
        Ok(output) => output.ok(),
        Err(RecvTimeoutError::Timeout) => {
            let _ = Command::new(KILL_PROGRAM)
                .args([KILL_SIGNAL, &pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = receiver.recv();
            None
        }
        Err(RecvTimeoutError::Disconnected) => None,
    }
}

/// Calls `check` every `interval` until it returns true or `limit` passes.
/// Use it only for an event with nothing to block on, such as a file appearing.
#[must_use]
pub fn poll_until(limit: Duration, interval: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while !check() {
        if Instant::now() >= deadline {
            return false;
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "the one polling seam for events with nothing to block on"
        )]
        thread::sleep(interval);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: Duration = Duration::from_millis(100);
    const GENEROUS: Duration = Duration::from_secs(60);
    const HANG_SECONDS: &str = "30";
    const KILL_BOUND: Duration = Duration::from_secs(10);
    const POLL: Duration = Duration::from_millis(5);

    fn spawn(program: &str, arg: &str) -> Child {
        Command::new(program)
            .arg(arg)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn output_within_returns_the_output_of_a_child_that_finishes() {
        // Arrange
        let child = spawn("echo", "hello");

        // Act
        let output = output_within(child, GENEROUS).unwrap();

        // Assert
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[test]
    fn output_within_kills_a_child_that_outlasts_the_limit() {
        // Arrange
        let child = spawn("sleep", HANG_SECONDS);
        let started = Instant::now();

        // Act
        let output = output_within(child, SHORT);

        // Assert
        assert!(output.is_none());
        assert!(started.elapsed() < KILL_BOUND);
    }

    #[test]
    fn poll_until_returns_true_once_the_check_passes() {
        // Arrange
        let mut calls = 0;

        // Act
        let ok = poll_until(GENEROUS, POLL, || {
            calls += 1;
            calls == 3
        });

        // Assert
        assert!(ok);
        assert_eq!(calls, 3);
    }

    #[test]
    fn poll_until_returns_false_when_the_limit_passes() {
        // Act
        let ok = poll_until(SHORT, POLL, || false);

        // Assert
        assert!(!ok);
    }
}
