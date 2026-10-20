// SPDX-License-Identifier: AGPL-3.0-only

//! Finds a backend binary and checks its version.

use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long `--version` may take before the binary is called unusable.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Most output kept from a probe; version lines are short.
const MAX_PROBE_OUTPUT: u64 = 1 << 20;
const VERSION_FLAG: &str = "--version";
const EXECUTABLE_BITS: u32 = 0o111;

/// A backend binary that was found and answered `--version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedBinary {
    /// Where the binary lives.
    pub path: PathBuf,
    /// The version it reported.
    pub version: String,
    /// Whether the version is on the verified allowlist.
    pub verified: bool,
}

/// The outcome of looking for a backend binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// No executable was found.
    Absent,
    /// A binary was found and reported a version.
    Found(DetectedBinary),
    /// A binary was found but did not report a usable version.
    Unusable {
        /// Where the binary lives.
        path: PathBuf,
        /// Why the version could not be read.
        reason: String,
    },
}

/// Where to look and what to accept.
#[derive(Debug, Clone, Copy)]
pub struct DetectRequest<'a> {
    /// File name to look for in each search directory.
    pub binary_name: &'a str,
    /// A configured path; used instead of searching when set.
    pub explicit: Option<&'a Path>,
    /// Directories to search in order; never read from the environment here.
    pub search_dirs: &'a [PathBuf],
    /// Versions known to work.
    pub verified: &'a [String],
    /// How long `--version` may take.
    pub probe_timeout: Duration,
}

/// Looks for the binary and runs `--version` on it.
#[must_use]
pub fn detect(request: &DetectRequest<'_>) -> Detection {
    let candidate = match request.explicit {
        Some(path) => is_executable_file(path).then(|| path.to_path_buf()),
        None => request
            .search_dirs
            .iter()
            .map(|dir| dir.join(request.binary_name))
            .find(|path| is_executable_file(path)),
    };
    let Some(path) = candidate else {
        return Detection::Absent;
    };
    match probe_version(&path, request.probe_timeout) {
        Ok(version) => {
            let verified = request.verified.contains(&version);
            Detection::Found(DetectedBinary {
                path,
                version,
                verified,
            })
        }
        Err(reason) => Detection::Unusable { path, reason },
    }
}

fn is_executable_file(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & EXECUTABLE_BITS != 0)
}

/// Runs `<path> --version` with an empty environment and a bounded wait.
///
/// Both streams are drained on their own threads so a chatty binary cannot
/// stall on a full pipe. Some CLIs print the version on stderr, so it is the
/// fallback when stdout holds no version.
fn probe_version(path: &Path, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(path)
        .arg(VERSION_FLAG)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("could not start: {err}"))?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // Best effort: the child may have exited between the check and the kill.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{VERSION_FLAG} did not finish in {timeout:?}"));
            }
            Ok(None) => std::thread::sleep(PROBE_POLL_INTERVAL),
            Err(err) => return Err(format!("could not wait for {VERSION_FLAG}: {err}")),
        }
    };
    if !status.success() {
        return Err(format!("{VERSION_FLAG} exited with {status}"));
    }
    let stdout = collect(stdout, deadline);
    let stderr = collect(stderr, deadline);
    parse_version(&stdout)
        .or_else(|| parse_version(&stderr))
        .ok_or_else(|| format!("{VERSION_FLAG} printed no version number"))
}

/// Reads a stream to its end on a thread, keeping the first
/// [`MAX_PROBE_OUTPUT`] bytes and discarding the rest so the writer never blocks.
fn drain<R: Read + Send + 'static>(mut stream: R) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        // A read error ends the stream; whatever arrived before it still counts.
        let _ = (&mut stream).take(MAX_PROBE_OUTPUT).read_to_end(&mut kept);
        let _ = io::copy(&mut stream, &mut io::sink());
        let _ = sender.send(String::from_utf8_lossy(&kept).into_owned());
    });
    receiver
}

/// Waits for a drained stream until `deadline`; a stream still held open by a
/// grandchild process yields nothing rather than blocking the probe.
fn collect(receiver: Option<mpsc::Receiver<String>>, deadline: Instant) -> String {
    receiver
        .and_then(|rx| {
            rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok()
        })
        .unwrap_or_default()
}

/// Extracts the first dotted version number from `--version` output, keeping a
/// prerelease or build suffix such as `-alpha.3`. The allowlist matches the whole
/// string exactly, and the Codex app-server's `thread.cliVersion` is checked
/// against the same list, so list the text both print.
#[must_use]
pub fn parse_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|word| word.trim_start_matches('v'))
        .find(|word| is_dotted_number(word))
        .map(str::to_owned)
}

fn is_dotted_number(word: &str) -> bool {
    let (core, suffix) = match word.split_once(['-', '+']) {
        Some((core, suffix)) => (core, Some(suffix)),
        None => (word, None),
    };
    let numeric = core.contains('.')
        && core
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let valid_suffix = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
    };
    numeric && suffix.is_none_or(valid_suffix)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// An upper bound, not a duration: starting a freshly written script can
    /// stall for many seconds on a loaded machine.
    const PATIENT_PROBE: Duration = Duration::from_secs(60);

    fn tool_dir(files: &[(&str, &str, u32)]) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body, mode) in files {
            let path = dir.path().join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(*mode)).unwrap();
        }
        dir
    }

    fn versioned(version_line: &str) -> String {
        format!("#!/bin/sh\necho '{version_line}'\n")
    }

    fn request<'a>(
        dirs: &'a [PathBuf],
        explicit: Option<&'a Path>,
        verified: &'a [String],
    ) -> DetectRequest<'a> {
        DetectRequest {
            binary_name: "claude",
            explicit,
            search_dirs: dirs,
            verified,
            probe_timeout: PATIENT_PROBE,
        }
    }

    #[test]
    fn parse_version_takes_the_first_dotted_number() {
        // Arrange
        let cases = [
            ("2.1.292 (Claude Code)", Some("2.1.292")),
            ("codex-cli 0.9.1\n", Some("0.9.1")),
            ("v1.2.3", Some("1.2.3")),
            ("codex-cli 0.1.0-alpha.3", Some("0.1.0-alpha.3")),
            ("tool 1.2.3+build.5 (x)", Some("1.2.3+build.5")),
            ("1.2.3- 1.2.4", Some("1.2.4")),
            ("no version here", None),
            ("", None),
        ];

        for (output, expected) in cases {
            // Act
            let version = parse_version(output);

            // Assert
            assert_eq!(version.as_deref(), expected, "output: {output:?}");
        }
    }

    #[test]
    fn finds_the_binary_in_a_search_dir_and_marks_a_verified_version() {
        // Arrange
        let dir = tool_dir(&[("claude", &versioned("2.1.292 (Claude Code)"), 0o755)]);
        let dirs = [dir.path().to_path_buf()];
        let verified = ["2.1.292".to_owned()];

        // Act
        let found = detect(&request(&dirs, None, &verified));

        // Assert
        assert_eq!(
            found,
            Detection::Found(DetectedBinary {
                path: dir.path().join("claude"),
                version: "2.1.292".to_owned(),
                verified: true,
            })
        );
    }

    #[test]
    fn a_version_off_the_allowlist_is_found_but_not_verified() {
        // Arrange
        let dir = tool_dir(&[("claude", &versioned("9.9.9 (Claude Code)"), 0o755)]);
        let dirs = [dir.path().to_path_buf()];
        let verified = ["2.1.292".to_owned()];

        // Act
        let found = detect(&request(&dirs, None, &verified));

        // Assert
        let Detection::Found(binary) = found else {
            panic!("expected Found, got {found:?}");
        };
        assert_eq!(binary.version, "9.9.9");
        assert!(!binary.verified);
    }

    #[test]
    fn an_absent_binary_is_reported_without_panicking() {
        // Arrange
        let empty = tool_dir(&[]);
        let dirs = [
            empty.path().to_path_buf(),
            PathBuf::from("/nonexistent-dir"),
        ];

        // Act
        let found = detect(&request(&dirs, None, &[]));

        // Assert
        assert_eq!(found, Detection::Absent);
    }

    #[test]
    fn a_non_executable_file_is_not_a_binary() {
        // Arrange
        let dir = tool_dir(&[("claude", &versioned("2.1.292"), 0o644)]);
        let dirs = [dir.path().to_path_buf()];

        // Act
        let found = detect(&request(&dirs, None, &[]));

        // Assert
        assert_eq!(found, Detection::Absent);
    }

    #[test]
    fn the_first_search_dir_with_the_binary_wins() {
        // Arrange
        let first = tool_dir(&[("claude", &versioned("1.0.0"), 0o755)]);
        let second = tool_dir(&[("claude", &versioned("2.0.0"), 0o755)]);
        let dirs = [first.path().to_path_buf(), second.path().to_path_buf()];

        // Act
        let found = detect(&request(&dirs, None, &[]));

        // Assert
        let Detection::Found(binary) = found else {
            panic!("expected Found, got {found:?}");
        };
        assert_eq!(binary.version, "1.0.0");
    }

    #[test]
    fn an_explicit_path_replaces_the_search() {
        // Arrange
        let searched = tool_dir(&[("claude", &versioned("1.0.0"), 0o755)]);
        let configured = tool_dir(&[("my-claude", &versioned("3.0.0"), 0o755)]);
        let dirs = [searched.path().to_path_buf()];
        let explicit = configured.path().join("my-claude");

        // Act
        let found = detect(&request(&dirs, Some(&explicit), &[]));

        // Assert
        let Detection::Found(binary) = found else {
            panic!("expected Found, got {found:?}");
        };
        assert_eq!(binary.path, explicit);
        assert_eq!(binary.version, "3.0.0");
    }

    #[test]
    fn a_missing_explicit_path_is_absent_and_does_not_fall_back_to_the_search() {
        // Arrange
        let searched = tool_dir(&[("claude", &versioned("1.0.0"), 0o755)]);
        let dirs = [searched.path().to_path_buf()];
        let explicit = searched.path().join("gone");

        // Act
        let found = detect(&request(&dirs, Some(&explicit), &[]));

        // Assert
        assert_eq!(found, Detection::Absent);
    }

    #[test]
    fn output_without_a_version_is_unusable() {
        // Arrange
        let dir = tool_dir(&[("claude", &versioned("hello"), 0o755)]);
        let dirs = [dir.path().to_path_buf()];

        // Act
        let found = detect(&request(&dirs, None, &[]));

        // Assert
        assert!(matches!(found, Detection::Unusable { .. }), "{found:?}");
    }

    #[test]
    fn a_binary_that_exits_nonzero_is_unusable() {
        // Arrange
        let dir = tool_dir(&[("claude", "#!/bin/sh\necho 2.1.292\nexit 3\n", 0o755)]);
        let dirs = [dir.path().to_path_buf()];

        // Act
        let found = detect(&request(&dirs, None, &[]));

        // Assert
        assert!(matches!(found, Detection::Unusable { .. }), "{found:?}");
    }

    #[test]
    fn a_binary_that_hangs_is_unusable_after_the_timeout() {
        // Arrange
        let dir = tool_dir(&[("claude", "#!/bin/sh\nexec sleep 30\n", 0o755)]);
        let dirs = [dir.path().to_path_buf()];
        let mut req = request(&dirs, None, &[]);
        req.probe_timeout = Duration::from_millis(100);

        // Act
        let found = detect(&req);

        // Assert
        assert!(matches!(found, Detection::Unusable { .. }), "{found:?}");
    }

    #[test]
    fn output_larger_than_the_pipe_buffer_does_not_hang_the_probe() {
        // Arrange
        let script = "#!/bin/sh\nhead -c 200000 /dev/zero | tr '\\0' x\necho\necho 2.1.292\n";
        let dir = tool_dir(&[("claude", script, 0o755)]);
        let dirs = [dir.path().to_path_buf()];

        // Act
        let found = detect(&request(&dirs, None, &[]));

        // Assert
        let Detection::Found(binary) = found else {
            panic!("expected Found, got {found:?}");
        };
        assert_eq!(binary.version, "2.1.292");
    }

    #[test]
    fn a_version_printed_on_stderr_is_read() {
        // Arrange
        let dir = tool_dir(&[("claude", "#!/bin/sh\necho 4.5.6 >&2\n", 0o755)]);
        let dirs = [dir.path().to_path_buf()];

        // Act
        let found = detect(&request(&dirs, None, &[]));

        // Assert
        let Detection::Found(binary) = found else {
            panic!("expected Found, got {found:?}");
        };
        assert_eq!(binary.version, "4.5.6");
    }
}
