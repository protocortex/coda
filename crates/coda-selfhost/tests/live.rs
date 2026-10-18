// SPDX-License-Identifier: AGPL-3.0-only

//! The live loop's handling of an attempt that outlives its time limit.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use coda_selfhost::attempt::Provider;
use coda_selfhost::live::{LiveOptions, run_live};

#[test]
fn an_attempt_that_times_out_is_rejected_and_the_next_one_runs() {
    // Arrange
    let source = tempfile::tempdir().unwrap();
    fs::write(source.path().join("a.txt"), "a").unwrap();
    let setup = "git init -q && git add -A && git -c user.name=t -c user.email=t@t \
                 -c commit.gpgsign=false commit -q -m base";
    assert!(
        Command::new("sh")
            .args(["-c", setup])
            .current_dir(source.path())
            .status()
            .unwrap()
            .success()
    );
    let tools = tempfile::tempdir().unwrap();
    let coda = tools.path().join("slow-coda");
    fs::write(&coda, "#!/bin/sh\n[ \"$1\" = run ] && sleep 30\n").unwrap();
    fs::set_permissions(&coda, fs::Permissions::from_mode(0o755)).unwrap();
    let provider = |_: u32, _: &Path| Provider::Anthropic {
        api_key_env: "UNUSED_KEY".to_owned(),
    };
    let mut log = Vec::new();

    // Act
    let outcome = run_live(
        &LiveOptions {
            coda: &coda,
            source: source.path(),
            attempts: 2,
            provider: &provider,
            env: Vec::new(),
            cargo_test_args: Vec::new(),
            target_dir: None,
            attempt_limit: Duration::from_millis(300),
            cargo_limit: Duration::from_secs(1),
            transcripts: None,
        },
        &mut |line| log.push(line.to_owned()),
    );

    // Assert
    assert_eq!(outcome.unwrap(), None);
    let rejections = log.iter().filter(|l| l.contains("rejected")).count();
    assert_eq!(rejections, 2, "{log:?}");
}
