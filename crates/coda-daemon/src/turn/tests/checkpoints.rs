// SPDX-License-Identifier: Apache-2.0

//! Every turn leaves a snapshot of the working tree behind, in a private ref.

use std::process::Command;

use coda_store::TurnCheckpoint;

use super::{Rig, relaxed};

const NO_PERMISSION_TURN: &str = "tool-slow";
/// A coverage run makes the fake backend write one of these into the session
/// folder as it starts or ends, at no fixed point of a turn. It is not the
/// agent's change, so the repository ignores it.
const COVERAGE_DATA: &str = "*.profraw\n";

pub(super) fn git(rig: &Rig, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(rig.cwd.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

pub(super) fn repo_rig() -> Rig {
    let rig = Rig::new(relaxed(), Vec::new());
    git(&rig, &["init", "-q", "-b", "main"]);
    let exclude = rig.cwd.path().join(".git/info/exclude");
    let kept = std::fs::read_to_string(&exclude).unwrap_or_default();
    std::fs::write(&exclude, format!("{kept}{COVERAGE_DATA}")).unwrap();
    std::fs::write(rig.cwd.path().join("a.txt"), "one\n").unwrap();
    git(&rig, &["add", "."]);
    git(&rig, &["commit", "-q", "-m", "first"]);
    rig
}

pub(super) async fn checkpoints(rig: &Rig) -> Vec<TurnCheckpoint> {
    let session = rig.session.clone();
    rig.hub
        .with_store(move |store| store.turn_checkpoints(&session))
        .await
        .unwrap()
}

#[tokio::test]
async fn the_first_turn_records_a_baseline_and_an_end_snapshot_in_private_refs() {
    // Arrange
    let mut rig = repo_rig();
    let head = git(&rig, &["rev-parse", "HEAD"]);

    // Act
    rig.send(NO_PERMISSION_TURN).await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let found = checkpoints(&rig).await;
    assert_eq!(found.len(), 1);
    let session = rig.session.as_str();
    let turn = found[0].turn.as_str();
    assert_eq!(
        found[0].before.as_deref(),
        Some(format!("refs/coda/checkpoints/{session}/base").as_str())
    );
    assert_eq!(
        found[0].after.as_deref(),
        Some(format!("refs/coda/checkpoints/{session}/{turn}").as_str())
    );
    for reference in [&found[0].before, &found[0].after] {
        let parent = git(
            &rig,
            &["rev-parse", &format!("{}^", reference.as_ref().unwrap())],
        );
        assert_eq!(parent, head);
    }
    assert_eq!(git(&rig, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&rig, &["log", "--oneline"]).lines().count(), 1);
}

#[tokio::test]
async fn a_later_turn_records_only_an_end_snapshot() {
    // Arrange
    let mut rig = repo_rig();
    rig.send(NO_PERMISSION_TURN).await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Act
    std::fs::write(rig.cwd.path().join("b.txt"), "two\n").unwrap();
    rig.send(NO_PERMISSION_TURN).await;
    rig.wait_until_idle().await;

    // Assert
    let found = checkpoints(&rig).await;
    assert_eq!(found.len(), 2);
    assert!(found[0].before.is_some());
    assert_eq!(found[1].before, None);
    let second = found[1].after.as_deref().expect("second turn snapshot");
    assert_eq!(git(&rig, &["show", &format!("{second}:b.txt")]), "two\n");
}

#[tokio::test]
async fn a_folder_that_is_not_a_repository_records_nothing_and_the_turn_still_ends() {
    // Arrange
    let mut rig = Rig::new(relaxed(), Vec::new());

    // Act
    rig.send(NO_PERMISSION_TURN).await;
    rig.wait_for("result").await;
    rig.wait_until_idle().await;

    // Assert
    let found = checkpoints(&rig).await;
    assert_eq!(found.len(), 1);
    assert_eq!((&found[0].before, &found[0].after), (&None, &None));
}
