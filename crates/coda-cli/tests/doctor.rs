// SPDX-License-Identifier: AGPL-3.0-only

//! `coda doctor`: what each check reports for a healthy and a broken home, and
//! what `--repair` removes and, above all, what it leaves alone.

mod common;

use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use coda_config::config::Dirs;
use coda_daemon::lock::InstanceLock;
use coda_daemon::pidfile;
use coda_daemon::server::{Server, ServerConfig};
use coda_store::Store;
use common::{POLL, TestDaemon, WAIT, cli, context, short_dir};
use serde_json::Value;

const EXIT_FAILURES: u8 = 20;
const EXIT_WARNINGS: u8 = 21;
/// Byte offset of `PRAGMA user_version` in the SQLite file header.
const USER_VERSION_OFFSET: u64 = 60;
const CANARY: &str = "canary-9f3a7c1e-not-a-real-secret";
const CANARY_VAR: &str = "CODA_DOCTOR_CANARY";
const SLEEP: &str = "/bin/sleep";
const SLEEP_SECS: &str = "60";
/// A child forked while a doctor run holds the instance lock keeps the lock
/// until it execs, so spawning and running doctor never overlap in one process.
static SPAWN_OR_DOCTOR: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const REUSED_START: &str = "Thu Jan  1 00:00:00 1970";

/// Runs `coda doctor --json` with `flags` in-process.
async fn run_doctor(base: &Path, flags: &[&str]) -> common::Outcome {
    let _serial = SPAWN_OR_DOCTOR.lock().await;
    let mut args = vec!["coda", "doctor", "--json"];
    args.extend_from_slice(flags);
    cli(&context(base), &args, "").await
}

/// The exit code and the parsed JSON of [`run_doctor`].
async fn doctor(base: &Path, flags: &[&str]) -> (ExitCode, Value) {
    let outcome = run_doctor(base, flags).await;
    let json = serde_json::from_str(&outcome.out)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}\n{}", outcome.out, outcome.err));
    (outcome.code, json)
}

fn field<'a>(report: &'a Value, check: &str, key: &str) -> &'a str {
    let checks = report["checks"].as_array().unwrap();
    let found = checks.iter().find(|c| c["name"] == check);
    found.unwrap_or_else(|| panic!("no check {check} in {report}"))[key]
        .as_str()
        .unwrap()
}

fn status<'a>(report: &'a Value, check: &str) -> &'a str {
    field(report, check, "status")
}

fn detail<'a>(report: &'a Value, check: &str) -> &'a str {
    field(report, check, "detail")
}

fn repairs(report: &Value) -> usize {
    report["repairs"].as_array().unwrap().len()
}

fn seed_database(base: &Path) {
    drop(Store::open(&Dirs::from_base(base).data_dir()).unwrap());
}

/// A fresh home that already holds a database.
fn seeded_home() -> tempfile::TempDir {
    let dir = short_dir();
    seed_database(dir.path());
    dir
}

fn run_dir(base: &Path) -> PathBuf {
    let dir = Dirs::from_base(base).run_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn write_config(base: &Path, text: &str) {
    fs::write(Dirs::from_base(base).config_file(), text).unwrap();
}

/// A socket file nobody listens on, as a killed daemon leaves it.
fn leave_stale_socket(base: &Path) -> PathBuf {
    run_dir(base);
    let path = Dirs::from_base(base).socket_path();
    drop(UnixListener::bind(&path).unwrap());
    assert!(path.exists());
    path
}

fn lock_path(base: &Path) -> PathBuf {
    Dirs::from_base(base).socket_path().with_extension("lock")
}

/// A child in its own process group that dies with the test, even on failure.
struct Sleeper(Child);

impl Sleeper {
    async fn spawn() -> Self {
        let _serial = SPAWN_OR_DOCTOR.lock().await;
        let child = Command::new(SLEEP)
            .arg(SLEEP_SECS)
            .process_group(0)
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
    }

    fn is_alive(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }

    fn wait_for_death(&mut self) {
        let deadline = Instant::now() + WAIT;
        while self.is_alive() {
            assert!(
                Instant::now() < deadline,
                "the orphan was not killed in time"
            );
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn record_pidfile(base: &Path, child: &Sleeper) -> (PathBuf, pidfile::PidRecord) {
    let _serial = SPAWN_OR_DOCTOR.lock().await;
    let path = run_dir(base).join("s1.pid");
    let record = pidfile::write(&path, child.0.id(), Path::new(SLEEP))
        .await
        .unwrap();
    (path, record)
}

#[tokio::test]
async fn a_home_with_a_database_and_no_daemon_is_healthy() {
    // Arrange
    let dir = seeded_home();
    run_dir(dir.path());

    // Act
    let (code, report) = doctor(dir.path(), &[]).await;

    // Assert
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(report["healthy"], true);
    for check in [
        "socket", "lock", "orphans", "database", "schema", "config", "backends",
    ] {
        assert_eq!(status(&report, check), "ok", "{check}: {report}");
    }
    assert!(!lock_path(dir.path()).exists());
    let (repaired_code, repaired) = doctor(dir.path(), &["--repair"]).await;
    assert_eq!((repaired_code, repairs(&repaired)), (ExitCode::SUCCESS, 0));
}

#[tokio::test]
async fn a_missing_database_warns_and_a_corrupt_one_fails() {
    for (corrupt, expected, exit) in [
        (false, "warn", EXIT_WARNINGS),
        (true, "fail", EXIT_FAILURES),
    ] {
        // Arrange
        let dir = short_dir();
        if corrupt {
            seed_database(dir.path());
            let db = Dirs::from_base(dir.path())
                .data_dir()
                .join(coda_store::DB_FILE_NAME);
            fs::write(&db, vec![b'x'; 4096]).unwrap();
            fs::set_permissions(&db, fs::Permissions::from_mode(0o600)).unwrap();
        }

        // Act
        let (code, report) = doctor(dir.path(), &[]).await;

        // Assert
        assert_eq!(status(&report, "database"), expected, "{report}");
        assert_eq!(code, ExitCode::from(exit));
    }
}

#[tokio::test]
async fn a_schema_newer_than_the_binary_fails_and_an_older_one_warns() {
    for (version, expected, exit) in [
        (coda_store::SCHEMA_VERSION + 1, "fail", EXIT_FAILURES),
        (coda_store::SCHEMA_VERSION - 1, "warn", EXIT_WARNINGS),
    ] {
        // Arrange
        let dir = seeded_home();
        let db = Dirs::from_base(dir.path())
            .data_dir()
            .join(coda_store::DB_FILE_NAME);
        let mut file = OpenOptions::new().write(true).open(db).unwrap();
        file.seek(SeekFrom::Start(USER_VERSION_OFFSET)).unwrap();
        file.write_all(&version.to_be_bytes()).unwrap();
        drop(file);

        // Act
        let (code, report) = doctor(dir.path(), &[]).await;

        // Assert
        assert_eq!(status(&report, "schema"), expected, "{report}");
        assert_eq!(code, ExitCode::from(exit));
    }
}

#[tokio::test]
async fn a_database_release_newer_than_the_binary_fails_and_an_older_one_warns() {
    for (release, expected, exit) in [
        (Some("999.0.0"), "fail", EXIT_FAILURES),
        (None, "warn", EXIT_WARNINGS),
        (Some("not a version"), "fail", EXIT_FAILURES),
    ] {
        // Arrange
        let dir = seeded_home();
        let db = Dirs::from_base(dir.path())
            .data_dir()
            .join(coda_store::DB_FILE_NAME);
        let conn = rusqlite::Connection::open(db).unwrap();
        // No record at all stands for a database that predates release tracking.
        match release {
            Some(release) => conn
                .execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'last_release'",
                    [release],
                )
                .unwrap(),
            None => conn.execute("DELETE FROM meta", []).unwrap(),
        };
        drop(conn);

        // Act
        let (code, report) = doctor(dir.path(), &[]).await;

        // Assert
        assert_eq!(status(&report, "database release"), expected, "{report}");
        assert_eq!(code, ExitCode::from(exit));
    }
}

#[tokio::test]
async fn an_unparsable_config_fails_and_an_unknown_key_warns() {
    for (text, expected) in [("max_turns = [", "fail"), ("surprise = 1\n", "warn")] {
        // Arrange
        let dir = seeded_home();
        write_config(dir.path(), text);

        // Act
        let (_, report) = doctor(dir.path(), &[]).await;

        // Assert
        assert_eq!(status(&report, "config"), expected, "{report}");
    }
}

#[tokio::test]
async fn an_enabled_backend_without_its_binary_fails() {
    // Arrange
    let dir = seeded_home();
    write_config(
        dir.path(),
        "[providers.claude_code]\nenabled = true\nbinary = \"/nonexistent/claude\"\n",
    );

    // Act
    let (code, report) = doctor(dir.path(), &[]).await;

    // Assert
    assert_eq!(code, ExitCode::from(EXIT_FAILURES));
    assert_eq!(status(&report, "backends"), "fail");
    assert!(detail(&report, "backends").contains("claude_code"));
}

#[tokio::test]
async fn a_codex_version_off_the_allowlist_warns_until_it_is_added() {
    // Arrange
    let dir = seeded_home();
    let codex = coda_testkit::paths::fake_codex_path().unwrap();
    let section = |extra: &str| {
        format!(
            "[providers.codex]\nenabled = true\nbinary = {:?}\n{extra}",
            codex.display().to_string()
        )
    };
    write_config(dir.path(), &section(""));

    // Act
    let (code, unverified) = doctor(dir.path(), &[]).await;
    write_config(
        dir.path(),
        &section("verified_versions = [\"0.0.0-fake\"]\n"),
    );
    let (_, verified) = doctor(dir.path(), &[]).await;

    // Assert
    assert_eq!(code, ExitCode::from(EXIT_WARNINGS));
    assert_eq!(status(&unverified, "backends"), "warn");
    assert!(detail(&unverified, "backends").contains("0.0.0-fake"));
    assert_eq!(status(&verified, "backends"), "ok");
}

#[tokio::test]
async fn an_unheld_lock_file_is_healthy_after_a_clean_shutdown_and_fails_beside_a_stale_socket() {
    for (stale, expected, exit) in [(false, "ok", 0), (true, "fail", EXIT_FAILURES)] {
        // Arrange
        let dir = seeded_home();
        run_dir(dir.path());
        if stale {
            leave_stale_socket(dir.path());
        }
        let lock = lock_path(dir.path());
        File::create(&lock).unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();

        // Act
        let (code, report) = doctor(dir.path(), &[]).await;

        // Assert
        assert_eq!(status(&report, "lock"), expected, "{report}");
        assert_eq!(code, ExitCode::from(exit));
        assert_eq!(fs::metadata(&lock).unwrap().mode() & 0o777, 0o644);
    }
}

#[tokio::test]
async fn a_running_daemon_is_reported_and_repair_leaves_it_and_its_database_alone() {
    // Arrange
    let dir = short_dir();
    let daemon = TestDaemon::start(dir.path());

    // Act
    let (code, report) = doctor(dir.path(), &[]).await;
    let (repair_code, repaired) = doctor(dir.path(), &["--repair"]).await;
    let (_, after) = doctor(dir.path(), &[]).await;
    daemon.stop().await;

    // Assert
    assert_eq!((code, repair_code), (ExitCode::SUCCESS, ExitCode::SUCCESS));
    assert!(detail(&report, "socket").contains("answering"));
    assert!(detail(&report, "database").contains("daemon"));
    assert!(detail(&report, "schema").contains("daemon"));
    assert_eq!(repairs(&repaired), 0);
    assert!(detail(&after, "socket").contains("answering"));
}

#[tokio::test]
async fn repair_leaves_a_held_lock_alone_even_when_nothing_answers() {
    // Arrange
    let dir = seeded_home();
    let socket = leave_stale_socket(dir.path());
    let _held = InstanceLock::acquire(&lock_path(dir.path())).unwrap();

    // Act
    let (_, report) = doctor(dir.path(), &["--repair"]).await;

    // Assert
    assert_eq!(repairs(&report), 0);
    assert!(socket.exists());
    assert!(lock_path(dir.path()).exists());
}

#[tokio::test]
async fn repair_keeps_the_lock_file_so_a_daemon_started_after_it_still_excludes_a_second() {
    // Arrange
    let dir = seeded_home();
    let socket = leave_stale_socket(dir.path());
    File::create(lock_path(dir.path())).unwrap();

    // Act
    let (code, first) = doctor(dir.path(), &["--repair"]).await;
    let (second_code, second) = doctor(dir.path(), &["--repair"]).await;

    // Assert
    assert_eq!(code, ExitCode::SUCCESS, "{first}");
    assert_eq!(repairs(&first), 1);
    assert!(!socket.exists());
    assert!(lock_path(dir.path()).exists());
    assert_eq!(second_code, ExitCode::SUCCESS);
    assert_eq!(repairs(&second), 0);
    let daemon = TestDaemon::start(dir.path());
    let config = ServerConfig::new(socket, fs::metadata(dir.path()).unwrap().uid());
    assert!(
        Server::bind(config).is_err(),
        "a second daemon got the lock"
    );
    daemon.stop().await;
}

/// A home whose run directory records a live orphan, with the run directory set to `mode`.
async fn home_with_orphan(mode: u32) -> (tempfile::TempDir, Sleeper) {
    let dir = seeded_home();
    let orphan = Sleeper::spawn().await;
    record_pidfile(dir.path(), &orphan).await;
    fs::set_permissions(run_dir(dir.path()), fs::Permissions::from_mode(mode)).unwrap();
    (dir, orphan)
}

#[tokio::test]
async fn a_non_socket_at_the_socket_path_is_reported_by_hand_and_does_not_stop_the_reaping() {
    // Arrange
    let (dir, mut orphan) = home_with_orphan(0o700).await;
    let socket = Dirs::from_base(dir.path()).socket_path();
    std::os::unix::fs::symlink("/nonexistent", &socket).unwrap();

    // Act
    let outcome = run_doctor(dir.path(), &["--repair"]).await;

    // Assert
    let report: Value = serde_json::from_str(&outcome.out).unwrap();
    orphan.wait_for_death();
    assert!(socket.symlink_metadata().is_ok());
    assert!(outcome.err.contains("remove it by hand"), "{}", outcome.err);
    assert_eq!(status(&report, "socket"), "fail");
    assert!(!detail(&report, "socket").contains("--repair"));
}

#[tokio::test]
async fn repair_refuses_a_run_dir_other_users_can_enter_and_kills_nothing() {
    // Arrange
    let (dir, mut orphan) = home_with_orphan(0o777).await;

    // Act
    let outcome = run_doctor(dir.path(), &["--repair"]).await;

    // Assert
    assert!(outcome.err.contains("other users"), "{}", outcome.err);
    assert!(orphan.is_alive());
}

#[tokio::test]
async fn repair_kills_an_orphaned_child_that_its_pidfile_still_describes() {
    // Arrange
    let dir = seeded_home();
    let mut orphan = Sleeper::spawn().await;
    let (pidfile, _) = record_pidfile(dir.path(), &orphan).await;
    let (before, unrepaired) = doctor(dir.path(), &[]).await;

    // Act
    let (after, repaired) = doctor(dir.path(), &["--repair"]).await;

    // Assert
    assert_eq!(before, ExitCode::from(EXIT_FAILURES));
    assert_eq!(status(&unrepaired, "orphans"), "fail");
    assert_eq!(after, ExitCode::SUCCESS, "{repaired}");
    orphan.wait_for_death();
    assert!(!pidfile.exists());
}

#[tokio::test]
async fn repair_never_kills_a_process_that_reused_the_recorded_pid() {
    // Arrange
    let dir = seeded_home();
    let unrelated = Sleeper::spawn().await;
    let (pidfile, mut record) = record_pidfile(dir.path(), &unrelated).await;
    record.started_at = REUSED_START.to_owned();
    fs::write(&pidfile, serde_json::to_vec(&record).unwrap()).unwrap();

    // Act
    let (code, report) = doctor(dir.path(), &["--repair"]).await;

    // Assert
    assert_eq!(code, ExitCode::SUCCESS, "{report}");
    assert_eq!(repairs(&report), 1, "{report}");
    assert!(!report["repairs"][0].as_str().unwrap().contains("killed"));
    assert!(!pidfile.exists());
}

#[tokio::test]
async fn no_secret_value_reaches_the_output_of_the_real_binary() {
    // Arrange
    let dir = seeded_home();
    write_config(
        dir.path(),
        &format!(
            "[providers.codex]\nenabled = true\nbinary = \"/nonexistent/codex\"\napi_key_env = \"{CANARY_VAR}\"\n"
        ),
    );
    let serial = SPAWN_OR_DOCTOR.lock().await;
    let child = Command::new(env!("CARGO_BIN_EXE_coda"))
        .args(["doctor", "--repair"])
        .env("CODA_HOME", dir.path())
        .env(CANARY_VAR, CANARY)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(serial);

    // Act
    let output = wait_bounded(child);

    // Assert
    let text = String::from_utf8_lossy(&output.stdout).into_owned()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(!text.contains(CANARY), "{text}");
    assert!(!text.replace(CANARY_VAR, "").contains("canary"), "{text}");
    assert_eq!(
        output.status.code(),
        Some(i32::from(EXIT_FAILURES)),
        "{text}"
    );
}

#[tokio::test]
async fn a_plaintext_secret_in_the_config_is_reported_without_its_value() {
    // Arrange
    let dir = seeded_home();
    write_config(
        dir.path(),
        &format!("[providers.codex]\napi_key = \"{CANARY}\"\n"),
    );
    let ctx = context(dir.path());

    // Act
    let outcome = cli(&ctx, &["coda", "doctor"], "").await;

    // Assert
    assert!(!outcome.out.contains(CANARY) && !outcome.err.contains(CANARY));
    assert!(
        outcome.out.contains("providers.codex.api_key"),
        "{}",
        outcome.out
    );
}

/// Waits for `child` to exit, killing it when it outlives [`WAIT`].
fn wait_bounded(mut child: Child) -> std::process::Output {
    let deadline = Instant::now() + WAIT;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}
