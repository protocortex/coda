// SPDX-License-Identifier: AGPL-3.0-only

//! Which named agents a `task` call may run, and with which tools.

use std::fs;

use coda_agents::{Catalog, DispatchError, child_tools, trust_project};
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
