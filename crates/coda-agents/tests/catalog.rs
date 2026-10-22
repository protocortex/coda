// SPDX-License-Identifier: AGPL-3.0-only

//! Loading: precedence between user and project files, and the trust gate.

use std::fs;
use std::path::Path;

use coda_agents::Catalog;
use tempfile::TempDir;

const MAX_BYTES: usize = 64 * 1024;
const SKILL: &str = "+++\nname = \"s\"\ndescription = \"d\"\n+++\nb";
const COMMAND: &str = "+++\nname = \"c\"\ndescription = \"d\"\n+++\nb";

fn agent(name: &str, prompt: &str, tools: &str) -> String {
    format!(
        "+++\nname = \"{name}\"\n\
         description = \"Does {name}. Use when asked. Do not use otherwise.\"\n\
         tier = \"fast\"\neffort = \"low\"\ntools = [{tools}]\n+++\n{prompt}\n"
    )
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

struct Fixture {
    user: TempDir,
    repo: TempDir,
}

fn fixture() -> Fixture {
    Fixture {
        user: TempDir::new().unwrap(),
        repo: TempDir::new().unwrap(),
    }
}

#[test]
fn user_files_load_without_any_project() {
    // Arrange
    let user = TempDir::new().unwrap();
    write(
        user.path(),
        "agents/scout.md",
        &agent("scout", "u", "\"read\""),
    );
    write(user.path(), "skills/s.md", SKILL);
    write(user.path(), "commands/c.md", COMMAND);

    // Act
    let catalog = Catalog::load(user.path(), None);

    // Assert
    assert_eq!(catalog.agents().count(), 1);
    assert_eq!(catalog.skills().count(), 1);
    assert_eq!(catalog.commands().count(), 1);
    assert_eq!(catalog.pending(), []);
}

#[test]
fn an_invalid_file_is_reported_and_does_not_hide_the_others() {
    // Arrange
    let f = fixture();
    write(f.user.path(), "agents/bad.md", "no frontmatter");
    write(
        f.user.path(),
        "agents/scout.md",
        &agent("scout", "u", "\"read\""),
    );

    // Act
    let catalog = Catalog::load(f.user.path(), Some(f.repo.path()));

    // Assert
    assert!(catalog.agent("scout").is_some());
    assert_eq!(catalog.problems().len(), 1);
    assert!(catalog.problems()[0].to_string().contains("bad.md"));
}

#[test]
fn project_definitions_are_always_pending_and_never_returned() {
    // Arrange
    let f = fixture();
    write(
        f.user.path(),
        "agents/scout.md",
        &agent("scout", "user prompt", "\"read\""),
    );
    write(
        f.repo.path(),
        ".coda/agents/scout.md",
        &agent("scout", "evil", "\"read\""),
    );
    write(
        f.repo.path(),
        ".coda/agents/local.md",
        &agent("local", "p", "\"read\""),
    );
    write(f.repo.path(), ".coda/skills/s.md", SKILL);
    write(f.repo.path(), ".coda/commands/c.md", COMMAND);

    // Act
    let catalog = Catalog::load(f.user.path(), Some(f.repo.path()));

    // Assert
    assert!(catalog.agent("local").is_none());
    assert_eq!(
        catalog.agent("scout").unwrap().def.body.trim(),
        "user prompt"
    );
    assert!(catalog.skill("s").is_none());
    assert!(catalog.command("c").is_none());
    assert_eq!(catalog.pending().len(), 4);
}

#[cfg(unix)]
#[test]
fn a_symlinked_file_is_refused() {
    // Arrange
    let user = TempDir::new().unwrap();
    let elsewhere = TempDir::new().unwrap();
    write(
        elsewhere.path(),
        "real.md",
        &agent("scout", "u", "\"read\""),
    );
    fs::create_dir(user.path().join("agents")).unwrap();
    std::os::unix::fs::symlink(
        elsewhere.path().join("real.md"),
        user.path().join("agents/scout.md"),
    )
    .unwrap();

    // Act
    let catalog = Catalog::load(user.path(), None);

    // Assert
    assert!(catalog.agent("scout").is_none());
    assert!(catalog.problems()[0].message.contains("not a regular file"));
}

#[test]
fn a_file_over_the_size_cap_is_refused_and_one_at_the_cap_loads() {
    // Arrange
    let user = TempDir::new().unwrap();
    let padded = |name: &str, total: usize| {
        let base = agent(name, "", "\"read\"");
        format!("{base}{}", "x".repeat(total - base.len()))
    };
    write(user.path(), "agents/big.md", &padded("big", MAX_BYTES + 1));
    write(user.path(), "agents/edge.md", &padded("edge", MAX_BYTES));

    // Act
    let catalog = Catalog::load(user.path(), None);

    // Assert
    assert!(catalog.agent("big").is_none());
    assert!(catalog.agent("edge").is_some());
    assert!(catalog.problems()[0].message.contains("larger than"));
}

#[test]
fn windows_line_endings_load() {
    // Arrange
    let user = TempDir::new().unwrap();
    write(
        user.path(),
        "agents/scout.md",
        &agent("scout", "line one\nline two", "\"read\"").replace('\n', "\r\n"),
    );

    // Act
    let catalog = Catalog::load(user.path(), None);

    // Assert
    let scout = catalog.agent("scout").unwrap();
    assert_eq!(scout.def.tools, ["read"]);
    assert!(scout.def.body.starts_with("line one"));
}

#[test]
fn a_leading_byte_order_mark_is_ignored() {
    // Arrange
    let user = TempDir::new().unwrap();
    write(
        user.path(),
        "agents/scout.md",
        &format!("\u{feff}{}", agent("scout", "u", "\"read\"")),
    );

    // Act
    let catalog = Catalog::load(user.path(), None);

    // Assert
    assert!(catalog.agent("scout").is_some(), "{:?}", catalog.problems());
}
