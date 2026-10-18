// SPDX-License-Identifier: AGPL-3.0-only

//! Child processes with a hard time bound.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(25);

/// A finished child.
#[derive(Debug)]
pub struct Captured {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("could not run {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("{program} did not finish within {limit:?} and was killed")]
    Timeout { program: String, limit: Duration },
    #[error("interrupted by a signal; {program} was killed")]
    Interrupted { program: String },
}

fn interrupt_flag() -> &'static Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

/// Makes SIGINT and SIGTERM stop the run instead of the process. Children are
/// in their own process groups, so the terminal's Ctrl-C never reaches them:
/// the run has to kill them itself and clean up before it exits.
pub fn handle_interrupts() -> std::io::Result<()> {
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(interrupt_flag()))?;
    }
    Ok(())
}

fn interrupted() -> bool {
    interrupt_flag().load(Ordering::Relaxed)
}

/// Runs `command` with no stdin, in its own process group, and kills the
/// whole group if it outlives `limit` or a signal handled by
/// [`handle_interrupts`] arrives.
pub fn run_bounded(command: &mut Command, limit: Duration) -> Result<Captured, ProcessError> {
    run(command, limit, true)
}

/// Like [`run_bounded`] but ignores interrupts, for the cleanup that must
/// still happen after one.
pub fn run_cleanup(command: &mut Command, limit: Duration) -> Result<Captured, ProcessError> {
    run(command, limit, false)
}

fn run(
    command: &mut Command,
    limit: Duration,
    stop_on_interrupt: bool,
) -> Result<Captured, ProcessError> {
    let program = command.get_program().to_string_lossy().into_owned();
    if stop_on_interrupt && interrupted() {
        return Err(ProcessError::Interrupted { program });
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|source| ProcessError::Spawn {
            program: program.clone(),
            source,
        })?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline && !(stop_on_interrupt && interrupted()) => {
                thread::sleep(POLL);
            }
            _ => {
                let was_interrupted = stop_on_interrupt && interrupted();
                kill_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                return Err(if was_interrupted {
                    ProcessError::Interrupted { program }
                } else {
                    ProcessError::Timeout { program, limit }
                });
            }
        }
    };
    // A background process may still hold the pipes open; reading would hang.
    kill_group(child.id());
    Ok(Captured {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// Reads a pipe on its own thread so a chatty child cannot fill it and stall.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    })
}

/// Stops the child and anything it started; a negative pid addresses the group.
fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_and_exit_status_are_captured() {
        // Act
        let done = run_bounded(
            Command::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]),
            Duration::from_secs(10),
        )
        .unwrap();

        // Assert
        assert_eq!(done.status.code(), Some(3));
        assert_eq!(
            (done.stdout.as_str(), done.stderr.as_str()),
            ("out\n", "err\n")
        );
    }

    #[test]
    fn a_background_process_holding_the_pipe_does_not_stall_the_read() {
        // Arrange
        let started = Instant::now();

        // Act
        let done = run_bounded(
            Command::new("sh").args(["-c", "sleep 8 & echo out"]),
            Duration::from_secs(30),
        )
        .unwrap();

        // Assert
        assert_eq!(done.stdout, "out\n");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn a_child_that_outlives_the_limit_is_killed_and_a_missing_program_is_a_spawn_error() {
        // Act
        let slow = run_bounded(
            Command::new("sh").args(["-c", "sleep 30 & wait"]),
            Duration::from_millis(200),
        );
        let missing = run_bounded(&mut Command::new("no-such-program"), Duration::from_secs(1));

        // Assert
        assert!(matches!(slow, Err(ProcessError::Timeout { .. })));
        assert!(matches!(missing, Err(ProcessError::Spawn { .. })));
    }
}
