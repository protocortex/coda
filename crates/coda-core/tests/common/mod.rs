// SPDX-License-Identifier: AGPL-3.0-only

//! Helpers for the hook tests: small scripts in a temporary directory, hook
//! entries, log capture and bounded polling.

#![allow(dead_code)]

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use coda_config::hooks::{HookConfig, HookEvent};
use serde_json::Value;
use tempfile::TempDir;
use tracing_subscriber::Registry;

/// An upper bound, not a duration: it only matters when the machine is badly loaded.
pub const BOUND: Duration = Duration::from_secs(20);
pub const POLL: Duration = Duration::from_millis(20);
/// The first run of a new script can take a second on a busy machine, so a hook
/// that must finish gets far longer than it needs.
pub const PATIENT_MS: u64 = 15_000;
/// For a hook that must start, and write what it started, before it times out.
pub const STARTED_MS: u64 = 3_000;

/// A temporary directory for hook scripts and what they write.
pub struct Scratch {
    pub dir: TempDir,
}

impl Scratch {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// Where a hook script writes what it saw.
    pub fn out(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// An executable script made of `body`, written into the directory.
    pub fn script(&self, name: &str, body: &str) -> String {
        let path = self.out(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }
}

/// A hook entry with a generous time limit.
pub fn hook(event: HookEvent, command: &[&str], blocking: bool) -> HookConfig {
    HookConfig {
        event,
        command: command.iter().map(|part| (*part).to_owned()).collect(),
        blocking,
        timeout_ms: Some(PATIENT_MS),
    }
}

/// `hook` with a different time limit.
pub fn timed(mut hook: HookConfig, timeout_ms: u64) -> HookConfig {
    hook.timeout_ms = Some(timeout_ms);
    hook
}

/// A hook that runs `script` through `/bin/sh -c`, which starts at once,
/// unlike a script written a moment ago.
pub fn shell(event: HookEvent, script: &str, blocking: bool) -> HookConfig {
    hook(event, &["/bin/sh", "-c", script], blocking)
}

pub fn json_file(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

pub async fn poll_until(mut done: impl FnMut() -> bool) {
    tokio::time::timeout(BOUND, async {
        while !done() {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("condition not reached in time");
}

/// The process id a hook wrote to `file`, once it has written all of it.
pub async fn pid_from(file: &Path) -> String {
    let mut pid = String::new();
    poll_until(|| {
        pid = std::fs::read_to_string(file).unwrap_or_default();
        pid.ends_with('\n')
    })
    .await;
    pid.trim().to_owned()
}

pub fn process_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Collects what the tracing subscriber writes.
#[derive(Clone, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

impl io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Logs {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// With a single live subscriber, tracing rebuilds callsite interest from the
/// calling thread's default alone, so a parallel test hitting an event with no
/// subscriber caches it as disabled for the whole process. A process-wide
/// subscriber keeps every callsite enabled.
pub fn capture_logs() -> (Logs, tracing::subscriber::DefaultGuard) {
    let _ = tracing::subscriber::set_global_default(Registry::default());
    tracing::callsite::rebuild_interest_cache();
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer({
            let logs = logs.clone();
            move || logs.clone()
        })
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}
