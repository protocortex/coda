// SPDX-License-Identifier: AGPL-3.0-only

//! Loading: precedence between user and project files, and the trust gate.

use std::fs;
use std::path::Path;

use coda_agents::{Catalog, CommandLookupError, PROJECT_DIR, PendingReason, Source, trust_project};
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
fn a_pending_file_is_named_by_its_file_name_without_parsing_it() {
    // Arrange
    let f = fixture();
    write(
        f.repo.path(),
        ".coda/commands/deploy.md",
        "not even a definition",
    );

    // Act
    let catalog = f.untrusted();

    // Assert
    let names: Vec<_> = catalog.pending().iter().map(|p| p.name()).collect();
    assert_eq!(names, [Some("deploy")]);
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
    let from_user = catalog.agents().filter(|e| e.source == Source::User);
    assert_eq!(from_user.count(), 1);
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

fn command(name: &str) -> String {
    format!("+++\nname = \"{name}\"\ndescription = \"d\"\n+++\nDo $ARGUMENTS\n")
}

#[test]
fn a_user_command_and_a_trusted_project_command_are_runnable() {
    // Arrange
    let f = fixture();
    write(f.user.path(), "commands/review.md", &command("review"));
    write(
        f.repo.path(),
        ".coda/commands/deploy.md",
        &command("deploy"),
    );

    // Act
    let catalog = f.trusted();

    // Assert
    let review = catalog.runnable_command("review").unwrap();
    assert_eq!(review.def.name, "review");
    assert_eq!(review.source, Source::User);
    let deploy = catalog.runnable_command("deploy").unwrap();
    assert_eq!(deploy.source, Source::Project);
}

#[test]
fn a_project_command_waiting_for_trust_is_refused_with_the_reason_and_the_way_forward() {
    // Arrange
    let f = fixture();
    write(
        f.repo.path(),
        ".coda/commands/deploy.md",
        &command("deploy"),
    );

    // Act
    let refused = f.untrusted().runnable_command("deploy").unwrap_err();

    // Assert
    assert_eq!(
        refused,
        CommandLookupError::Pending {
            name: "deploy".to_owned(),
            reason: PendingReason::NotTrusted
        }
    );
    assert_eq!(
        refused.to_string(),
        "deploy is a project command that is not in use: the repository is not trusted; \
         review the files, then run `coda agents trust`"
    );
}

#[test]
fn a_command_file_edited_after_trust_is_refused_as_changed() {
    // Arrange
    let f = fixture();
    write(
        f.repo.path(),
        ".coda/commands/deploy.md",
        &command("deploy"),
    );
    let mut store = Store::open(&f.data.path().join("data")).unwrap();
    trust_project(&mut store, f.repo.path()).unwrap();
    let edited = command("deploy").replace("Do", "Undo");
    write(f.repo.path(), ".coda/commands/deploy.md", &edited);

    // Act
    let catalog = Catalog::load_trusted(&store, f.user.path(), Some(f.repo.path())).unwrap();

    // Assert
    let refused = catalog.runnable_command("deploy").unwrap_err();
    assert!(matches!(
        refused,
        CommandLookupError::Pending {
            reason: PendingReason::ChangedSinceTrust,
            ..
        }
    ));
}

#[test]
fn a_name_no_file_defines_is_unknown_and_a_pending_agent_file_is_not_a_command() {
    // Arrange
    let f = fixture();
    write(
        f.repo.path(),
        ".coda/agents/deploy.md",
        &agent("deploy", "p", "\"read\""),
    );

    // Act
    let catalog = f.untrusted();

    // Assert
    for name in ["nope", "deploy", "Deploy", ""] {
        assert_eq!(
            catalog.runnable_command(name).unwrap_err(),
            CommandLookupError::Unknown {
                name: name.to_owned()
            },
            "{name:?}"
        );
    }
}

#[test]
fn a_user_command_runs_even_when_a_project_file_of_the_same_name_waits_for_trust() {
    // Arrange
    let f = fixture();
    write(f.user.path(), "commands/deploy.md", &command("deploy"));
    write(
        f.repo.path(),
        ".coda/commands/deploy.md",
        &command("deploy"),
    );

    // Act
    let catalog = f.untrusted();

    // Assert
    let deploy = catalog.runnable_command("deploy").unwrap();
    assert_eq!(deploy.source, Source::User);
}

#[test]
fn problems_are_summed_up_in_a_line_that_names_the_lint_command() {
    // Arrange
    let f = fixture();
    write(f.user.path(), "agents/bad.md", "no frontmatter");
    write(f.user.path(), "commands/worse.md", "no frontmatter either");

    // Act
    let hint = f.untrusted().problems_hint();

    // Assert
    let hint = hint.unwrap();
    assert!(hint.starts_with("2 definition files"), "{hint}");
    assert!(hint.contains("`coda agents lint`"), "{hint}");
}

#[test]
fn several_problems_in_one_file_count_as_one_file_and_a_clean_catalog_has_no_hint() {
    // Arrange
    let f = fixture();
    let two_problems = agent("bad", "p", "\"teleport\"").replace("\"fast\"", "\"turbo\"");
    write(f.user.path(), "agents/bad.md", &two_problems);

    // Act
    let catalog = f.untrusted();

    // Assert
    assert!(catalog.problems().len() > 1, "{:?}", catalog.problems());
    let hint = catalog.problems_hint().unwrap();
    assert!(hint.starts_with("1 definition file "), "{hint}");
    assert_eq!(fixture().untrusted().problems_hint(), None);
}
