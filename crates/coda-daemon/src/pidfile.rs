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
    #[error("the start time of process {pid} is unavailable")]
    StartTimeUnavailable { pid: u32 },
}

/// The start time of `pid`, or `None` when no such process exists.
///
/// Read through `ps`, which reports whole seconds; a pid recycled within the
/// same second as the original start would pass the check in [`is_same_process`].
pub async fn process_start_time(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    let started = String::from_utf8(output.stdout).ok()?;
    let started = started.trim();
    (output.status.success() && !started.is_empty()).then(|| started.to_owned())
}

/// Writes the pidfile for a running child.
///
/// # Errors
/// Returns [`PidfileError::StartTimeUnavailable`] when the process is gone and
/// [`PidfileError::Io`] when the file cannot be written.
pub async fn write(path: &Path, pid: u32, exe: &Path) -> Result<PidRecord, PidfileError> {
    let started_at = process_start_time(pid)
        .await
        .ok_or(PidfileError::StartTimeUnavailable { pid })?;
    let record = PidRecord {
        pid,
        started_at,
        exe: exe.to_path_buf(),
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
///
/// Only the name is compared: `ps` may report just the name, and a launcher
/// script or symlink can make the full paths differ for the same program.
async fn process_exe_name(pid: u32) -> Option<std::ffi::OsString> {
    if let Ok(exe) = fs::read_link(format!("/proc/{pid}/exe")) {
        return exe.file_name().map(ToOwned::to_owned);
    }
    let output = Command::new("ps")
        .args(["-o", "comm=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;
    let comm = String::from_utf8(output.stdout).ok()?;
    let comm = comm.trim();
    (output.status.success() && !comm.is_empty())
        .then(|| Path::new(comm).file_name().map(ToOwned::to_owned))
        .flatten()
}

/// Whether the recorded pid is alive, started when the record says, and still
/// runs the recorded executable.
pub async fn is_same_process(record: &PidRecord) -> bool {
    process_start_time(record.pid).await.as_deref() == Some(record.started_at.as_str())
        && process_exe_name(record.pid).await.as_deref() == record.exe.file_name()
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
        let record = write(
            &dir.path().join("a.pid"),
            child.id().unwrap(),
            Path::new(EXE),
        )
        .await
        .unwrap();

        // Act
        let same = is_same_process(&record).await;

        // Assert
        assert!(!same);
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
