// SPDX-License-Identifier: AGPL-3.0-only

//! The `coda-selfhost` binary the shell scripts call.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use coda_selfhost::task::{MAX_TURNS, TOKEN_CAP};

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

const KEY_VAR: &str = "SELFHOST_TEST_KEY";
const CANARY: &str = "canary-9b2d5e77";

fn git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    fs::write(repo.path().join("a.txt"), "a").unwrap();
    let setup = "git init -q && git add -A && git -c user.name=t -c user.email=t@t \
                 -c commit.gpgsign=false commit -q -m base";
    assert!(
        Command::new("sh")
            .args(["-c", setup])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    repo
}

/// An executable `#!/bin/sh` script named `name` in `dir`.
fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A stand-in for `coda` that prints the key variable, its working directory,
/// its arguments and the `config.toml` it was given.
fn fake_coda(dir: &Path) -> std::path::PathBuf {
    script(
        dir,
        "fake-coda",
        &format!("echo \"key=${KEY_VAR} cwd=$PWD args=$*\"\ncat \"$CODA_HOME/config.toml\""),
    )
}

fn live(repo: &Path, coda: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .args(["live", "--attempts", "1", "--api-key-env", KEY_VAR])
        .arg("--coda")
        .arg(coda)
        .arg("--source")
        .arg(repo)
        .args(extra)
        .env(KEY_VAR, CANARY)
        .output()
        .unwrap()
}

#[test]
fn live_refuses_to_start_without_the_key_variable() {
    // Arrange
    let repo = git_repo();

    // Act
    let output = Command::new(BIN)
        .args(["live", "--api-key-env", KEY_VAR, "--coda", "/no/such/coda"])
        .arg("--write-transcript")
        .arg("--source")
        .arg(repo.path())
        .env_remove(KEY_VAR)
        .output()
        .unwrap();

    // Assert
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3));
    assert!(
        stderr.contains(KEY_VAR) && stderr.contains("is not set"),
        "{stderr}"
    );
    assert!(!stderr.contains("could not run"), "{stderr}");
    assert!(!repo.path().join("docs/selfhost").exists());
}

#[test]
fn transcripts_are_written_only_with_the_flag_and_hide_the_key() {
    // Arrange
    let repo = git_repo();
    let tools = tempfile::tempdir().unwrap();
    let coda = fake_coda(tools.path());
    let transcript = repo.path().join("docs/selfhost/attempt-1.txt");

    // Act
    let without = live(repo.path(), &coda, &[]);
    let missing_without = !transcript.exists();
    let with = live(repo.path(), &coda, &["--write-transcript"]);

    // Assert
    assert_eq!(without.status.code(), Some(1));
    assert_eq!(with.status.code(), Some(1));
    assert!(missing_without);
    let written = fs::read_to_string(&transcript).unwrap();
    assert!(
        written.contains(&format!("key=<env:{KEY_VAR}>")),
        "{written}"
    );
    assert!(written.contains("cwd=<path>"), "{written}");
    assert!(
        written.contains(&format!("api_key_env = \"{KEY_VAR}\"")),
        "{written}"
    );
    assert!(
        written.contains("[providers.anthropic]") && !written.contains("faux"),
        "{written}"
    );
    for flag in [
        format!("--max-turns {MAX_TURNS}"),
        format!("--token-cap {TOKEN_CAP}"),
    ] {
        assert!(written.contains(&flag), "{flag} missing from {written}");
    }
    for stream in [&without.stdout, &without.stderr, &with.stdout, &with.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains(CANARY));
    }
    assert!(!written.contains(CANARY));
}

#[test]
fn scrub_prints_a_file_without_environment_values_or_paths() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("raw.txt");
    fs::write(&file, format!("token {CANARY} in /Users/jo/repo\n")).unwrap();

    // Act
    let output = Command::new(BIN)
        .arg("scrub")
        .arg(&file)
        .env(KEY_VAR, CANARY)
        .output()
        .unwrap();

    // Assert
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("token <env:{KEY_VAR}> in <path>\n")
    );
}

#[test]
fn scrub_jsonl_rejects_a_line_that_is_not_json_and_prints_nothing() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("raw.jsonl");
    fs::write(&file, "{\"a\":1}\n{\"b\":\n").unwrap();

    // Act
    let output = Command::new(BIN)
        .args(["scrub", "--jsonl"])
        .arg(&file)
        .output()
        .unwrap();

    // Assert
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    assert!(String::from_utf8_lossy(&output.stderr).contains("line 2"));
}

const OTHER_VAR: &str = "SELFHOST_TEST_OTHER";
const OTHER: &str = "other-canary-5521";

/// A repository with a `coda-cli` package, so the verifier's cargo can run in
/// a copy of it.
fn cargo_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let package = repo.path().join("crates/coda-cli");
    fs::create_dir_all(package.join("src")).unwrap();
    fs::write(
        repo.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/coda-cli\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    fs::write(
        package.join("Cargo.toml"),
        "[package]\nname = \"coda-cli\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(package.join("src/main.rs"), "fn main() {}\n").unwrap();
    let setup = "git init -q && git add -A && git -c user.name=t -c user.email=t@t \
                 -c commit.gpgsign=false commit -q -m base";
    assert!(
        Command::new("sh")
            .args(["-c", setup])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success()
    );
    repo
}

/// Adds a test to `main.rs` and a build script that dumps the environment to
/// `$DUMP_PATH` and then fails with the value of `$SELFHOST_TEST_OTHER`.
const EDIT_CARGO_REPO: &str = r#"cat >> crates/coda-cli/src/main.rs <<'RS'
#[test]
fn added() {}
RS
cat > crates/coda-cli/build.rs <<'RS'
fn main() {
    let env: Vec<(String, String)> = std::env::vars().collect();
    std::fs::write(std::env::var("DUMP_PATH").unwrap(), format!("{env:?}")).unwrap();
    panic!("build failed for {}", std::env::var("SELFHOST_TEST_OTHER").unwrap());
}
RS"#;

#[test]
fn the_model_edited_build_never_sees_the_api_key_and_the_log_hides_other_secrets() {
    // Arrange
    let repo = cargo_repo();
    let tools = tempfile::tempdir().unwrap();
    let coda = script(tools.path(), "edit-coda", EDIT_CARGO_REPO);
    let dump = tools.path().join("env-dump.txt");

    // Act
    let output = Command::new(BIN)
        .args(["live", "--attempts", "1", "--api-key-env", KEY_VAR])
        .arg("--coda")
        .arg(&coda)
        .arg("--source")
        .arg(repo.path())
        .env(KEY_VAR, CANARY)
        .env(OTHER_VAR, OTHER)
        .env("DUMP_PATH", &dump)
        .output()
        .unwrap();

    // Assert
    let stderr = String::from_utf8_lossy(&output.stderr);
    let dumped = fs::read_to_string(&dump).unwrap_or_default();
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(dumped.contains("CODA_SELFHOST_NESTED"), "build did not run");
    assert!(!dumped.contains(CANARY), "the api key reached the build");
    assert!(stderr.contains("rejected"), "{stderr}");
    assert!(!stderr.contains(OTHER), "{stderr}");
    assert!(stderr.contains(&format!("<env:{OTHER_VAR}>")), "{stderr}");
}

#[test]
fn verify_hides_environment_values_in_the_rejection_reason() {
    // Arrange
    let repo = cargo_repo();
    let dump = repo.path().join("env-dump.txt");
    assert!(
        Command::new("sh")
            .args(["-c", EDIT_CARGO_REPO])
            .env(OTHER_VAR, OTHER)
            .env("DUMP_PATH", &dump)
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
        .env(OTHER_VAR, OTHER)
        .env("DUMP_PATH", &dump)
        .output()
        .unwrap();

    // Assert
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(!stderr.contains(OTHER), "{stderr}");
    assert!(stderr.contains(&format!("<env:{OTHER_VAR}>")), "{stderr}");
}

#[test]
fn a_failure_to_run_prints_without_environment_values() {
    // Arrange
    let repo = git_repo();
    let coda = format!("/no/such/{CANARY}/coda");

    // Act
    let output = live(repo.path(), Path::new(&coda), &[]);

    // Assert
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    assert!(stderr.contains("could not run"), "{stderr}");
    assert!(!stderr.contains(CANARY), "{stderr}");
}

/// An upper bound, not a duration: copying the workspace and starting a
/// process can stall for a long time on a loaded machine.
const PROCESS_BOUND: Duration = Duration::from_secs(120);

fn wait_for(path: &Path) -> String {
    let deadline = Instant::now() + PROCESS_BOUND;
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            return text.trim().to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "{} never appeared",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn process_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn an_interrupt_kills_the_running_coda_and_removes_the_temporary_root() {
    for signal in ["INT", "TERM"] {
        // Arrange
        let repo = git_repo();
        let tools = tempfile::tempdir().unwrap();
        let mark = tools.path();
        let coda = script(
            mark,
            "sleepy-coda",
            &format!(
                "[ \"$1\" = run ] || exit 0\npwd > {m}/pwd.tmp && mv {m}/pwd.tmp {m}/pwd\nsleep 300 &\n\
                 echo $! > {m}/pid.tmp && mv {m}/pid.tmp {m}/pid\nwait",
                m = mark.display()
            ),
        );
        let mut child = Command::new(BIN)
            .args(["live", "--attempts", "1", "--api-key-env", KEY_VAR])
            .arg("--coda")
            .arg(&coda)
            .arg("--source")
            .arg(repo.path())
            .env(KEY_VAR, CANARY)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let sleeper = wait_for(&mark.join("pid"));
        let copy = wait_for(&mark.join("pwd"));

        // Act
        Command::new("kill")
            .arg(format!("-{signal}"))
            .arg(child.id().to_string())
            .status()
            .unwrap();
        let deadline = Instant::now() + PROCESS_BOUND;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = Command::new("kill").args(["-KILL", &sleeper]).status();
                panic!("live did not exit after SIG{signal}");
            }
            std::thread::sleep(Duration::from_millis(25));
        };

        // Assert
        assert_eq!(status.code(), Some(130), "SIG{signal}");
        assert!(!process_alive(&sleeper), "SIG{signal}: coda survived");
        let root = Path::new(&copy).parent().unwrap();
        assert!(!root.exists(), "SIG{signal}: {} remains", root.display());
    }
}

#[test]
fn transcripts_refuse_to_start_over_stale_files_and_a_symlinked_directory() {
    // Arrange
    let tools = tempfile::tempdir().unwrap();
    let coda = script(tools.path(), "marker-coda", "touch \"$MARKER\"");
    let marker = tools.path().join("ran");
    let stale_repo = git_repo();
    let stale_dir = stale_repo.path().join("docs/selfhost");
    fs::create_dir_all(&stale_dir).unwrap();
    fs::write(stale_dir.join("attempt-2.txt"), "old").unwrap();
    let linked_repo = git_repo();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(linked_repo.path().join("docs")).unwrap();
    std::os::unix::fs::symlink(outside.path(), linked_repo.path().join("docs/selfhost")).unwrap();
    let run = |repo: &Path| {
        Command::new(BIN)
            .args(["live", "--attempts", "1", "--api-key-env", KEY_VAR])
            .args(["--write-transcript", "--coda"])
            .arg(&coda)
            .arg("--source")
            .arg(repo)
            .env(KEY_VAR, CANARY)
            .env("MARKER", &marker)
            .output()
            .unwrap()
    };

    // Act
    let (stale, linked) = (run(stale_repo.path()), run(linked_repo.path()));

    // Assert
    let stale_err = String::from_utf8_lossy(&stale.stderr);
    assert_eq!(stale.status.code(), Some(3), "{stale_err}");
    assert!(stale_err.contains("already exist"), "{stale_err}");
    assert_eq!(
        fs::read_to_string(stale_dir.join("attempt-2.txt")).unwrap(),
        "old"
    );
    assert_eq!(linked.status.code(), Some(3));
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    assert!(!marker.exists(), "coda ran before the refusal");
}

#[test]
fn a_symlink_planted_during_the_run_is_replaced_not_followed() {
    // Arrange
    let repo = git_repo();
    let tools = tempfile::tempdir().unwrap();
    let victim = tools.path().join("victim.txt");
    fs::write(&victim, "victim").unwrap();
    let coda = script(
        tools.path(),
        "planting-coda",
        "mkdir -p \"$SRC/docs/selfhost\" && ln -s \"$VICTIM\" \"$SRC/docs/selfhost/attempt-1.txt\"",
    );

    // Act
    let output = Command::new(BIN)
        .args(["live", "--attempts", "1", "--api-key-env", KEY_VAR])
        .args(["--write-transcript", "--coda"])
        .arg(&coda)
        .arg("--source")
        .arg(repo.path())
        .env(KEY_VAR, CANARY)
        .env("SRC", repo.path())
        .env("VICTIM", &victim)
        .output()
        .unwrap();

    // Assert
    let dir = repo.path().join("docs/selfhost");
    let written = dir.join("attempt-1.txt");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&victim).unwrap(), "victim");
    assert!(fs::symlink_metadata(&written).unwrap().is_file());
    let names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        names,
        ["attempt-1.txt"],
        "stray files in the transcript directory"
    );
}
