// SPDX-License-Identifier: AGPL-3.0-only

//! Which named agents a `task` call may run, and with which tools.

use std::fs;

use coda_agents::{Catalog, DispatchError, Source, child_tools, trust_project};
use coda_store::Store;
use tempfile::TempDir;

fn agent(name: &str, tools: &str) -> String {
    format!(
        "+++\nname = \"{name}\"\n\
         description = \"Does {name}. Use when asked. Do not use otherwise.\"\n\
         tier = \"fast\"\neffort = \"low\"\ntools = [{tools}]\n+++\nprompt\n"
    )
}

/// A catalog with the user agents `a` and `b` (with `tools`) and the project
/// agent `local`, loaded with the repository trusted or not.
fn catalog(trusted: bool, tools: &str) -> Catalog {
    let (user, repo, data) = (
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    );
    fs::create_dir_all(user.path().join("agents")).unwrap();
    fs::create_dir_all(repo.path().join(".coda/agents")).unwrap();
    for name in ["a", "b"] {
        fs::write(
            user.path().join(format!("agents/{name}.md")),
            agent(name, tools),
        )
        .unwrap();
    }
    fs::write(
        repo.path().join(".coda/agents/local.md"),
        agent("local", tools),
    )
    .unwrap();
    let mut store = Store::open(&data.path().join("data")).unwrap();
    if trusted {
        trust_project(&mut store, repo.path()).unwrap();
    }
    Catalog::load_trusted(&store, user.path(), Some(repo.path())).unwrap()
}

#[test]
fn a_user_agent_can_be_dispatched_and_a_project_agent_only_once_trusted() {
    // Arrange
    let (before, after) = (catalog(false, "\"read\""), catalog(true, "\"read\""));

    // Act
    let user = before.dispatchable("a");
    let refused = before.dispatchable("local");
    let allowed = after.dispatchable("local");

    // Assert
    assert_eq!(user.unwrap().def.name, "a");
    assert!(matches!(refused, Err(DispatchError::NotTrusted { ref name, .. }) if name == "local"));
    assert!(
        refused
            .unwrap_err()
            .to_string()
            .contains("coda agents trust")
    );
    assert_eq!(allowed.unwrap().def.name, "local");
}

#[test]
fn an_unknown_agent_is_refused_and_the_available_ones_are_named() {
    // Arrange
    let catalog = catalog(false, "\"read\"");

    // Act
    let refused = catalog.dispatchable("nope");

    // Assert
    let mut available: Vec<String> = Catalog::builtin()
        .agents()
        .map(|entry| entry.def.name.clone())
        .chain(["a", "b"].map(str::to_owned))
        .collect();
    available.sort();
    assert_eq!(
        refused.unwrap_err(),
        DispatchError::Unknown {
            name: "nope".to_owned(),
            available
        }
    );
}

/// A catalog of the user's `agents/` files and, once trusted, the project's
/// `.coda/agents/` files, given as `(file name, text)`.
fn catalog_of(user_files: &[(&str, String)], project_files: &[(&str, String)]) -> Catalog {
    let (user, repo, data) = (
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    );
    for (dir, files) in [
        (user.path().join("agents"), user_files),
        (repo.path().join(".coda/agents"), project_files),
    ] {
        fs::create_dir_all(&dir).unwrap();
        for (file, text) in files {
            fs::write(dir.join(file), text).unwrap();
        }
    }
    let mut store = Store::open(&data.path().join("data")).unwrap();
    trust_project(&mut store, repo.path()).unwrap();
    Catalog::load_trusted(&store, user.path(), Some(repo.path())).unwrap()
}

#[test]
fn a_skipped_file_named_like_a_built_in_refuses_the_dispatch_with_the_problem() {
    // Arrange
    let broken = agent("implementer", "\"nope\"");
    let catalog = catalog_of(&[("implementer.md", broken)], &[]);

    // Act
    let refused = catalog.dispatchable("implementer");

    // Assert
    let Err(DispatchError::Skipped { name, problems }) = refused else {
        panic!("the built-in ran in its place: {refused:?}");
    };
    assert_eq!(name, "implementer");
    assert_eq!(problems.len(), 1);
    let text = DispatchError::Skipped { name, problems }.to_string();
    assert!(text.contains("implementer.md"), "{text}");
    assert!(text.contains("unknown tool `nope`"), "{text}");
    assert!(text.contains("coda agents lint"), "{text}");
}

#[test]
fn a_skipped_user_file_still_refuses_when_a_valid_project_file_has_the_name() {
    // Arrange
    let broken = agent("reviewer", "\"nope\"");
    let fine = agent("reviewer", "\"read\"");
    let catalog = catalog_of(&[("reviewer.md", broken)], &[("reviewer.md", fine)]);

    // Act
    let refused = catalog.dispatchable("reviewer");

    // Assert
    assert!(
        matches!(refused, Err(DispatchError::Skipped { .. })),
        "{refused:?}"
    );
}

#[test]
fn a_project_file_that_only_lost_a_setting_still_runs() {
    // Arrange
    let asks_for_more = agent("reviewer", "\"read\", \"bash\"");
    let catalog = catalog_of(&[], &[("reviewer.md", asks_for_more)]);

    // Act
    let allowed = catalog.dispatchable("reviewer");

    // Assert
    assert!(
        !catalog.problems().is_empty(),
        "the ignored tool is reported"
    );
    assert_eq!(allowed.unwrap().source, Source::Project);
}

#[test]
fn a_problem_in_another_file_does_not_stop_an_agent() {
    // Arrange
    let other = agent("ghost", "\"nope\"");
    let command_like = "+++\nbroken";
    let catalog = catalog_of(&[("ghost.md", other)], &[]);
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("commands")).unwrap();
    fs::write(dir.path().join("commands/explore.md"), command_like).unwrap();
    let with_command = Catalog::load(dir.path(), None);

    // Act
    let (from_agents, from_commands) = (
        catalog.dispatchable("explore"),
        with_command.dispatchable("explore"),
    );

    // Assert
    assert!(!catalog.problems().is_empty() && !with_command.problems().is_empty());
    assert_eq!(from_agents.unwrap().def.name, "explore");
    assert_eq!(from_commands.unwrap().def.name, "explore");
}

#[test]
fn a_child_gets_the_agents_tools_that_the_parent_also_has() {
    // Arrange
    let catalog = catalog(false, "\"read\", \"grep\", \"bash\"");
    let def = &catalog.dispatchable("a").unwrap().def;

    // Act
    let tools = child_tools(def, &["grep", "read", "edit"]);

    // Assert
    assert_eq!(tools, ["read", "grep"]);
}
