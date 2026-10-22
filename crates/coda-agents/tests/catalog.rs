// SPDX-License-Identifier: AGPL-3.0-only

//! Loading: precedence between user and project files, and the trust gate.

use std::fs;
use std::path::Path;

use coda_agents::{Catalog, PROJECT_DIR, Source, trust_project};
use coda_store::Store;
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
    data: TempDir,
}

fn fixture() -> Fixture {
    Fixture {
        user: TempDir::new().unwrap(),
        repo: TempDir::new().unwrap(),
        data: TempDir::new().unwrap(),
    }
}

impl Fixture {
    /// Loads with the project files as they are now recorded as trusted.
    fn trusted(&self) -> Catalog {
        let mut store = Store::open(&self.data.path().join("data")).unwrap();
        trust_project(&mut store, self.repo.path()).unwrap();
        Catalog::load_trusted(&store, self.user.path(), Some(self.repo.path())).unwrap()
    }

    fn untrusted(&self) -> Catalog {
        Catalog::load(self.user.path(), Some(self.repo.path()))
    }
}

#[test]
fn a_project_agent_overrides_the_user_prompt_but_cannot_widen_tools() {
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
        &agent("scout", "project prompt", "\"read\", \"bash\""),
    );

    // Act
    let catalog = f.trusted();

    // Assert
    let scout = catalog.agent("scout").unwrap();
    assert_eq!(scout.def.body.trim(), "project prompt");
    assert_eq!(scout.def.tools, ["read"]);
    assert_eq!(scout.source, Source::Project);
    assert!(
        catalog
            .problems()
            .iter()
            .any(|p| p.to_string().contains("bash"))
    );
}

#[test]
fn a_project_agent_may_narrow_the_tools_of_a_user_agent() {
    // Arrange
    let f = fixture();
    write(
        f.user.path(),
        "agents/scout.md",
        &agent("scout", "u", "\"read\", \"grep\""),
    );
    write(
        f.repo.path(),
        ".coda/agents/scout.md",
        &agent("scout", "p", "\"grep\""),
    );

    // Act
    let catalog = f.trusted();

    // Assert
    assert_eq!(catalog.agent("scout").unwrap().def.tools, ["grep"]);
}

#[test]
fn a_project_only_agent_is_usable_once_trusted() {
    // Arrange
    let f = fixture();
    write(
        f.repo.path(),
        ".coda/agents/local.md",
        &agent("local", "p", "\"read\""),
    );

    // Act
    let catalog = f.trusted();

    // Assert
    assert_eq!(catalog.agent("local").unwrap().source, Source::Project);
}

#[test]
fn a_hook_key_in_a_project_file_is_rejected_and_the_file_is_not_loaded() {
    // Arrange
    let f = fixture();
    let text = agent("local", "p", "\"read\"").replace("+++\np", "hooks = [\"x\"]\n+++\np");
    write(f.repo.path(), ".coda/agents/local.md", &text);

    // Act
    let catalog = f.trusted();

    // Assert
    assert!(catalog.agent("local").is_none());
    assert!(
        catalog
            .problems()
            .iter()
            .any(|p| p.to_string().contains("hooks"))
    );
}

#[test]
fn untrusted_project_files_are_pending_and_never_returned() {
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
    let catalog = f.untrusted();

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
    let catalog = f.untrusted();

    // Assert
    assert!(catalog.agent("scout").is_some());
    assert_eq!(catalog.problems().len(), 1);
    assert!(catalog.problems()[0].to_string().contains("bad.md"));
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

#[cfg(unix)]
#[test]
fn a_symlinked_coda_directory_is_refused() {
    // Arrange
    let f = fixture();
    let elsewhere = TempDir::new().unwrap();
    write(
        elsewhere.path(),
        "agents/local.md",
        &agent("local", "p", "\"read\""),
    );
    std::os::unix::fs::symlink(elsewhere.path(), f.repo.path().join(PROJECT_DIR)).unwrap();

    // Act
    let catalog = f.trusted();

    // Assert
    assert!(catalog.agent("local").is_none());
    assert!(
        catalog
            .problems()
            .iter()
            .any(|p| p.message.contains("symlink"))
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_subfolder_is_refused() {
    // Arrange
    let f = fixture();
    let elsewhere = TempDir::new().unwrap();
    write(
        elsewhere.path(),
        "local.md",
        &agent("local", "p", "\"read\""),
    );
    fs::create_dir(f.repo.path().join(PROJECT_DIR)).unwrap();
    std::os::unix::fs::symlink(
        elsewhere.path(),
        f.repo.path().join(PROJECT_DIR).join("agents"),
    )
    .unwrap();

    // Act
    let catalog = f.trusted();

    // Assert
    assert!(catalog.agent("local").is_none());
    assert!(
        catalog
            .problems()
            .iter()
            .any(|p| p.message.contains("symlink"))
    );
}
