// SPDX-License-Identifier: AGPL-3.0-only

//! The self-hosting acceptance regression: the scripted faux provider makes
//! the pinned change through the real `coda run` and daemon in a temporary
//! copy of this repository, and the verifier judges the result.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use coda_selfhost::NESTED_ENV;
use coda_selfhost::attempt::{AttemptSpec, Provider, run_attempt};
use coda_selfhost::error::SelfhostError;
use coda_selfhost::live::{LiveOptions, run_live};
use coda_selfhost::process::ProcessError;
use coda_selfhost::task::{
    MAIN_NEW, MAIN_OLD, TEST_NEW, TEST_OLD, faux_script, manifest_edit_script,
};
use coda_selfhost::verify::{MAIN_RS, VerifyError, VerifyPlan, verify_workdir};
use coda_selfhost::workspace::prepare_copy;
use common::short_dir;
use tempfile::TempDir;

const VERSION_TEST_RS: &str = "crates/coda-cli/tests/version.rs";
const TARGET_SUBDIR: &str = "selfhost";
const LOCK_FILE: &str = "selfhost.lock";
const ATTEMPT_LIMIT: Duration = Duration::from_secs(120);
const CARGO_LIMIT: Duration = Duration::from_mins(15);

/// A repository copy and a daemon home, both removed on drop.
struct Sandbox {
    _dir: TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = short_dir();
        let root = dir.path().canonicalize().unwrap();
        let repo = root.join("repo");
        let home = root.join("home");
        fs::create_dir(&home).unwrap();
        prepare_copy(&source_root(), &repo).unwrap();
        Self {
            _dir: dir,
            home,
            repo,
        }
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.repo.join(relative)).unwrap()
    }

    fn write(&self, relative: &str, content: &str) {
        fs::write(self.repo.join(relative), content).unwrap();
    }

    fn attempt(&self, script: &str) {
        let script_path = self.home.join("script.json");
        fs::write(&script_path, script).unwrap();
        let transcript = run_attempt(&AttemptSpec {
            coda: Path::new(env!("CARGO_BIN_EXE_coda")),
            home: &self.home,
            workdir: &self.repo,
            provider: Provider::Faux {
                script: script_path,
            },
            limit: ATTEMPT_LIMIT,
        })
        .unwrap();
        assert_eq!(transcript.exit_code, Some(0), "{}", transcript.stderr);
    }

    fn verify(&self) -> Result<(), SelfhostError> {
        let target = target_dir();
        let _lock = lock_target(&target);
        verify_workdir(&VerifyPlan {
            workdir: &self.repo,
            cargo_test_args: ["-p", "coda-cli", "--test", "version"]
                .map(str::to_owned)
                .to_vec(),
            target_dir: Some(&target),
            limit: CARGO_LIMIT,
            withheld_env: Vec::new(),
        })
    }
}

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The repository file with the pinned edit applied by plain replacement.
fn edited(relative: &str, old: &str, new: &str) -> String {
    let original = fs::read_to_string(source_root().join(relative)).unwrap();
    assert!(
        original.contains(old),
        "{relative} no longer has the text to edit"
    );
    original.replacen(old, new, 1)
}

fn expected_main() -> String {
    edited(MAIN_RS, MAIN_OLD, MAIN_NEW)
}

fn expected_test() -> String {
    edited(VERSION_TEST_RS, TEST_OLD, TEST_NEW)
}

/// A build directory of its own, kept between runs so dependencies are built
/// once: sharing the workspace's would overwrite the `coda` binary its tests run.
fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| source_root().join("target"), PathBuf::from)
        .join(TARGET_SUBDIR)
}

/// The scenarios build different sources into one output directory, so they
/// take turns; the lock is released when the file is dropped.
fn lock_target(target: &Path) -> fs::File {
    fs::create_dir_all(target).unwrap();
    let lock = fs::File::create(target.join(LOCK_FILE)).unwrap();
    lock.lock().unwrap();
    lock
}

/// The verifier's own `cargo test` runs this file again; it must not recurse.
fn nested() -> bool {
    std::env::var_os(NESTED_ENV).is_some()
}

fn rejected(result: Result<(), SelfhostError>) -> VerifyError {
    match result {
        Err(SelfhostError::Rejected(why)) => why,
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[test]
fn the_scripted_attempt_makes_the_exact_pinned_change_and_passes_the_verifier() {
    if nested() {
        return;
    }
    // Arrange
    let sandbox = Sandbox::new();

    // Act
    sandbox.attempt(&faux_script());
    let verdict = sandbox.verify();

    // Assert
    assert_eq!(sandbox.read(MAIN_RS), expected_main());
    assert_eq!(sandbox.read(VERSION_TEST_RS), expected_test());
    verdict.unwrap();
}

#[test]
fn an_edit_to_an_execution_affecting_path_is_denied_and_the_attempt_fails() {
    if nested() {
        return;
    }
    // Arrange
    let sandbox = Sandbox::new();
    let manifest = sandbox.read("Cargo.toml");

    // Act
    sandbox.attempt(&manifest_edit_script());

    // Assert
    assert_eq!(sandbox.read("Cargo.toml"), manifest);
    assert_eq!(rejected(sandbox.verify()), VerifyError::NoDiff);
}

#[test]
fn the_verifier_rejects_no_diff_no_test_and_the_wrong_json() {
    if nested() {
        return;
    }
    // Arrange
    let wrong_name = |content: &str| content.replace("env!(\"CARGO_PKG_NAME\")", "\"coda\"");
    let mismatch = VerifyError::Mismatch {
        field: "name",
        found: "coda".to_owned(),
        expected: "coda-cli".to_owned(),
    };
    let cases = [
        (vec![], VerifyError::NoDiff),
        (vec![(MAIN_RS, expected_main())], VerifyError::NoTestAdded),
        (
            vec![
                (MAIN_RS, wrong_name(&expected_main())),
                (VERSION_TEST_RS, wrong_name(&expected_test())),
            ],
            mismatch,
        ),
    ];
    for (files, expected) in cases {
        let sandbox = Sandbox::new();
        for (path, content) in files {
            sandbox.write(path, &content);
        }

        // Act and Assert
        assert_eq!(rejected(sandbox.verify()), expected);
    }
}

const CANARY_VAR: &str = "SELFHOST_CANARY_KEY";
const CANARY: &str = "canary-7f3a9c41";

/// A faux script whose only reply is `text`: it changes nothing.
fn chatter_script(text: &str) -> String {
    serde_json::json!([{"events": [
        {"type": "text_delta", "text": text},
        {"type": "stop", "reason": "end_turn"},
    ]}])
    .to_string()
}

struct LiveRun {
    outcome: Result<Option<u32>, SelfhostError>,
    log: Vec<String>,
    out: PathBuf,
    _dir: TempDir,
}

/// Runs the live loop with the faux provider; `scripts(n)` is attempt `n`'s script.
fn live(coda: &Path, scripts: impl Fn(u32) -> String) -> LiveRun {
    let dir = short_dir();
    let out = dir.path().join("transcripts");
    let target = target_dir();
    let _lock = lock_target(&target);
    let mut log = Vec::new();
    let provider = |attempt: u32, home: &Path| {
        let script = home.join("script.json");
        fs::write(&script, scripts(attempt)).unwrap();
        Provider::Faux { script }
    };
    let outcome = run_live(
        &LiveOptions {
            coda,
            source: &source_root(),
            attempts: 3,
            provider: &provider,
            env: vec![(CANARY_VAR.to_owned(), CANARY.to_owned())],
            cargo_test_args: ["-p", "coda-cli", "--test", "version"]
                .map(str::to_owned)
                .to_vec(),
            target_dir: Some(&target),
            attempt_limit: ATTEMPT_LIMIT,
            cargo_limit: CARGO_LIMIT,
            transcripts: Some(&out),
        },
        &mut |line| log.push(line.to_owned()),
    );
    LiveRun {
        outcome,
        log,
        out,
        _dir: dir,
    }
}

#[test]
fn the_scripted_attempt_loop_stops_at_the_first_attempt_that_passes() {
    if nested() {
        return;
    }
    // Act
    let run = live(Path::new(env!("CARGO_BIN_EXE_coda")), |attempt| {
        if attempt == 1 {
            chatter_script("no change")
        } else {
            faux_script()
        }
    });

    // Assert
    assert_eq!(run.outcome.unwrap(), Some(2));
    assert!(run.out.join("attempt-1.txt").is_file());
    assert!(run.out.join("attempt-2.txt").is_file());
    assert!(!run.out.join("attempt-3.txt").exists());
    assert!(run.log.iter().all(|line| !line.contains("attempt 3")));
}

#[test]
fn when_every_attempt_is_rejected_the_loop_reports_none_and_the_transcripts_hide_the_key() {
    if nested() {
        return;
    }
    // Act
    let run = live(Path::new(env!("CARGO_BIN_EXE_coda")), |_| {
        chatter_script(&format!("my key is {CANARY}"))
    });

    // Assert
    assert_eq!(run.outcome.unwrap(), None);
    for attempt in 1..=3 {
        let transcript =
            fs::read_to_string(run.out.join(format!("attempt-{attempt}.txt"))).unwrap();
        assert!(
            transcript.contains(&format!("<env:{CANARY_VAR}>")),
            "{transcript}"
        );
        assert!(!transcript.contains(CANARY));
    }
    assert!(run.log.iter().all(|line| !line.contains(CANARY)));
}

#[test]
fn a_run_that_cannot_start_is_an_error_not_a_rejected_attempt() {
    if nested() {
        return;
    }
    // Act
    let run = live(Path::new("/no/such/coda"), |_| faux_script());

    // Assert
    assert!(matches!(
        run.outcome,
        Err(SelfhostError::Process(ProcessError::Spawn { .. }))
    ));
    assert!(!run.out.exists(), "a transcript was written");
    assert!(run.log.iter().all(|line| !line.contains("attempt 2")));
}
