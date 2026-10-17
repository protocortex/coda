// SPDX-License-Identifier: AGPL-3.0-only

//! Pidfiles that let a later daemon prove a recorded child is still the same process.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use rustix::fs::OFlags;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Owner read and write only: the file names a process the daemon may signal.
pub const PIDFILE_MODE: u32 = 0o600;

/// What a pidfile holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidRecord {
    pub pid: u32,
    /// The process start time as `ps` reports it.
    pub started_at: String,
    pub exe: PathBuf,
    /// The executable name the live process showed when the file was written.
    /// It can differ from `exe` for a symlink or an interpreter launcher.
    pub process_name: String,
}

/// What a recorded pid refers to now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// No process has this pid.
    Gone,
    /// The process the record describes.
    Same,
    /// A different process reuses the pid.
    Other,
}

/// Why a pidfile could not be written or read.
#[derive(Debug, thiserror::Error)]
pub enum PidfileError {
    #[error("pidfile {} could not be accessed: {source}", .path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("pidfile {} is not valid: {source}", .path.display())]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("the start time or name of process {pid} is unavailable")]
    StartTimeUnavailable { pid: u32 },
}

/// Reads one `ps` field for `pid`: `None` when no such process exists, an
/// error when `ps` itself could not tell.
async fn ps_field(field: &str, pid: u32) -> io::Result<Option<String>> {
    let output = Command::new("ps")
        .args(["-o", &format!("{field}="), "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let text = text.trim();
    if output.status.success() && !text.is_empty() {
        Ok(Some(text.to_owned()))
    } else if output.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(io::Error::other(format!(
            "ps exited with {}",
            output.status
        )))
    }
}

/// The start time of `pid`, or `None` when no such process exists.
///
/// Read through `ps`, which reports whole seconds; a pid recycled within the
/// same second as the original start would pass the check in [`is_same_process`].
pub async fn process_start_time(pid: u32) -> Option<String> {
    ps_field("lstart", pid).await.ok().flatten()
}

/// Writes the pidfile for a running child.
///
/// # Errors
/// Returns [`PidfileError::StartTimeUnavailable`] when the process is gone and
/// [`PidfileError::Io`] when the file cannot be written.
pub async fn write(path: &Path, pid: u32, exe: &Path) -> Result<PidRecord, PidfileError> {
    let unavailable = || PidfileError::StartTimeUnavailable { pid };
    let started_at = process_start_time(pid).await.ok_or_else(unavailable)?;
    let process_name = process_name(pid)
        .await
        .ok()
        .flatten()
        .ok_or_else(unavailable)?;
    let record = PidRecord {
        pid,
        started_at,
        exe: exe.to_path_buf(),
        process_name,
    };
    let io = |source| PidfileError::Io {
        path: path.to_path_buf(),
        source,
    };
    let body = serde_json::to_vec(&record).map_err(|source| PidfileError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    // Unlink first so a planted symlink is dropped, not written through; the
    // exclusive create with NOFOLLOW refuses one that reappears in between.
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(e)),
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PIDFILE_MODE)
        .custom_flags(OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .map_err(io)?;
    file.set_permissions(fs::Permissions::from_mode(PIDFILE_MODE))
        .map_err(io)?;
    file.write_all(&body).map_err(io)?;
    Ok(record)
}

/// Reads a pidfile.
///
/// # Errors
/// Returns [`PidfileError::Io`] or [`PidfileError::Parse`].
pub fn read(path: &Path) -> Result<PidRecord, PidfileError> {
    let body = fs::read(path).map_err(|source| PidfileError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&body).map_err(|source| PidfileError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// The file name of the executable `pid` runs, or `None` when no such process exists.
async fn process_name(pid: u32) -> io::Result<Option<String>> {
    if let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe")) {
        return Ok(exe.file_name().map(|n| n.to_string_lossy().into_owned()));
    }
    Ok(ps_field("comm", pid).await?.and_then(|comm| {
        Path::new(&comm)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }))
}

/// Compares the recorded pid with the live process.
///
/// # Errors
/// Returns the I/O error when `ps` cannot answer, so the caller can tell
/// "could not check" from "not the same process".
pub async fn check_process(record: &PidRecord) -> io::Result<ProcessState> {
    match ps_field("lstart", record.pid).await? {
        None => return Ok(ProcessState::Gone),
        Some(started) if started != record.started_at => return Ok(ProcessState::Other),
        Some(_) => {}
    }
    Ok(match process_name(record.pid).await? {
        None => ProcessState::Gone,
        Some(name) if name == record.process_name => ProcessState::Same,
        Some(_) => ProcessState::Other,
    })
}

/// Whether the recorded pid is alive, started when the record says, and still
/// runs the executable it ran when recorded.
pub async fn is_same_process(record: &PidRecord) -> bool {
    check_process(record).await.ok() == Some(ProcessState::Same)
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tokio::process::Command;

    use super::{PIDFILE_MODE, PidfileError, is_same_process, process_start_time, read, write};

    const EXE: &str = "/usr/bin/claude-test";
    const SLEEP: &str = "/bin/sleep";

    fn sleeper() -> tokio::process::Child {
        Command::new("sleep")
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    #[tokio::test]
    async fn the_pidfile_is_owner_only_even_over_a_wider_existing_file() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.pid");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        // Act
        write(&path, std::process::id(), Path::new(EXE))
            .await
            .unwrap();

        // Assert
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, PIDFILE_MODE);
    }

    #[tokio::test]
    async fn the_record_holds_pid_start_time_and_executable() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.pid");
        let pid = std::process::id();

        // Act
        let written = write(&path, pid, Path::new(EXE)).await.unwrap();
        let read_back = read(&path).unwrap();

        // Assert
        assert_eq!(read_back, written);
        assert_eq!(written.pid, pid);
        assert_eq!(written.exe, Path::new(EXE));
        assert_eq!(Some(written.started_at), process_start_time(pid).await);
    }

    #[tokio::test]
    async fn a_live_child_with_its_recorded_start_time_is_the_same_process() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let child = sleeper();
        let record = write(
            &dir.path().join("a.pid"),
            child.id().unwrap(),
            Path::new(SLEEP),
        )
        .await
        .unwrap();

        // Act
        let same = is_same_process(&record).await;

        // Assert
        assert!(same);
    }

    #[tokio::test]
    async fn a_reused_pid_with_another_start_time_is_not_the_same_process() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let child = sleeper();
        let mut record = write(
            &dir.path().join("a.pid"),
            child.id().unwrap(),
            Path::new(SLEEP),
        )
        .await
        .unwrap();
        record.started_at = "Thu Jan  1 00:00:00 1970".to_owned();

        // Act
        let same = is_same_process(&record).await;

        // Assert
        assert!(!same);
    }

    #[tokio::test]
    async fn a_reaped_child_is_not_the_same_process() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let mut child = sleeper();
        let record = write(
            &dir.path().join("a.pid"),
            child.id().unwrap(),
            Path::new(SLEEP),
        )
        .await
        .unwrap();
        child.kill().await.unwrap();

        // Act
        let same = is_same_process(&record).await;

        // Assert
        assert!(!same);
    }

    #[tokio::test]
    async fn writing_for_a_missing_process_fails() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let mut child = sleeper();
        let pid = child.id().unwrap();
        child.kill().await.unwrap();

        // Act
        let result = write(&dir.path().join("a.pid"), pid, Path::new(EXE)).await;

        // Assert
        assert!(matches!(
            result,
            Err(PidfileError::StartTimeUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn a_live_pid_running_another_executable_is_not_the_same_process() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let child = sleeper();
        let mut record = write(
            &dir.path().join("a.pid"),
            child.id().unwrap(),
            Path::new(SLEEP),
        )
        .await
        .unwrap();
        record.process_name = "claude".to_owned();

        // Act
        let same = is_same_process(&record).await;

        // Assert
        assert!(!same);
    }

    #[tokio::test]
    async fn a_symlinked_launcher_is_still_the_same_process() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("claude");
        std::os::unix::fs::symlink(SLEEP, &link).unwrap();
        let child = Command::new(&link)
            .arg("30")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let record = write(&dir.path().join("a.pid"), child.id().unwrap(), &link)
            .await
            .unwrap();

        // Act
        let same = is_same_process(&record).await;

        // Assert
        assert!(same);
    }

    #[tokio::test]
    async fn a_shebang_launcher_is_still_the_same_process() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("claude");
        fs::write(&script, "#!/bin/sh\nread line\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let child = Command::new(&script)
            .stdin(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let record = write(&dir.path().join("a.pid"), child.id().unwrap(), &script)
            .await
            .unwrap();

        // Act
        let same = is_same_process(&record).await;

        // Assert
        assert!(same);
    }

    #[tokio::test]
    async fn a_symlink_at_the_pidfile_path_is_replaced_not_followed() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("victim");
        fs::write(&target, "keep").unwrap();
        let path = dir.path().join("a.pid");
        std::os::unix::fs::symlink(&target, &path).unwrap();

        // Act
        write(&path, std::process::id(), Path::new(EXE))
            .await
            .unwrap();

        // Assert
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
        assert!(!fs::symlink_metadata(&path).unwrap().is_symlink());
        assert!(read(&path).is_ok());
    }

    #[test]
    fn a_malformed_pidfile_is_a_parse_error() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.pid");
        fs::write(&path, "not json").unwrap();

        // Act
        let result = read(&path);

        // Assert
        assert!(matches!(result, Err(PidfileError::Parse { .. })));
    }
}
