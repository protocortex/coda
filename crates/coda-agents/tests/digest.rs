// SPDX-License-Identifier: AGPL-3.0-only

//! Trust is tied to the bytes of the project definition files.

use std::fs;
use std::path::Path;

use coda_agents::{Catalog, PendingReason, revoke_project, trust_project};
use coda_store::Store;
use tempfile::TempDir;

const AGENT: &str = "+++\nname = \"scout\"\n\
    description = \"Finds code. Use when exploring. Do not use when editing.\"\n\
    tier = \"fast\"\neffort = \"low\"\ntools = [\"read\"]\n+++\nExplore.\n";

struct World {
    user: TempDir,
    repo: TempDir,
    store: Store,
    _data: TempDir,
}

fn world() -> World {
    let data = TempDir::new().unwrap();
    let store = Store::open(&data.path().join("data")).unwrap();
    let repo = TempDir::new().unwrap();
    write(repo.path(), ".coda/agents/scout.md", AGENT);
    World {
        user: TempDir::new().unwrap(),
        repo,
        store,
        _data: data,
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

impl World {
    fn load(&self) -> Catalog {
        Catalog::load_trusted(&self.store, self.user.path(), Some(self.repo.path())).unwrap()
    }
}

#[test]
fn a_repository_nobody_trusted_is_pending() {
    // Arrange
    let w = world();

    // Act
    let catalog = w.load();

    // Assert
    assert!(catalog.agent("scout").is_none());
    assert_eq!(catalog.pending()[0].reason, PendingReason::NotTrusted);
}

#[test]
fn trusted_files_load_until_one_is_edited() {
    // Arrange
    let mut w = world();
    trust_project(&mut w.store, w.repo.path()).unwrap();

    // Act
    let before = w.load();
    write(
        w.repo.path(),
        ".coda/agents/scout.md",
        &AGENT.replace("Explore.", "Exfiltrate."),
    );
    let after = w.load();

    // Assert
    assert!(before.agent("scout").is_some());
    assert!(after.agent("scout").is_none());
    assert_eq!(after.pending()[0].reason, PendingReason::ChangedSinceTrust);
}

#[test]
fn a_new_file_withdraws_trust_and_trusting_again_restores_it() {
    // Arrange
    let mut w = world();
    trust_project(&mut w.store, w.repo.path()).unwrap();
    write(
        w.repo.path(),
        ".coda/skills/s.md",
        "+++\nname = \"s\"\ndescription = \"d\"\n+++\nb",
    );

    // Act
    let added = w.load();
    trust_project(&mut w.store, w.repo.path()).unwrap();
    let retrusted = w.load();

    // Assert
    assert!(added.agent("scout").is_none());
    assert_eq!(added.pending().len(), 2);
    assert!(retrusted.agent("scout").is_some());
    assert!(retrusted.skill("s").is_some());
}

#[test]
fn a_removed_file_withdraws_trust() {
    // Arrange
    let mut w = world();
    write(
        w.repo.path(),
        ".coda/skills/s.md",
        "+++\nname = \"s\"\ndescription = \"d\"\n+++\nb",
    );
    trust_project(&mut w.store, w.repo.path()).unwrap();
    fs::remove_file(w.repo.path().join(".coda/skills/s.md")).unwrap();

    // Act
    let catalog = w.load();

    // Assert
    assert!(catalog.agent("scout").is_none());
}

#[test]
fn files_outside_the_definition_folders_do_not_withdraw_trust() {
    // Arrange
    let mut w = world();
    trust_project(&mut w.store, w.repo.path()).unwrap();

    // Act
    write(w.repo.path(), ".coda/notes.md", "anything");
    write(w.repo.path(), "README.md", "anything");

    // Assert
    assert!(w.load().agent("scout").is_some());
}

#[test]
fn revoking_makes_the_files_pending_again() {
    // Arrange
    let mut w = world();
    trust_project(&mut w.store, w.repo.path()).unwrap();

    // Act
    let existed = revoke_project(&mut w.store, w.repo.path()).unwrap();
    let catalog = w.load();

    // Assert
    assert!(existed);
    assert!(catalog.agent("scout").is_none());
    assert_eq!(catalog.pending()[0].reason, PendingReason::NotTrusted);
}
