// SPDX-License-Identifier: AGPL-3.0-only

//! The agents that ship in the binary, and how files override them.

use std::fs;
use std::path::Path;

use coda_agents::{
    Catalog, Effort, PendingReason, Severity, Source, Tier, parse_report, trust_project,
};
use coda_store::Store;
use tempfile::TempDir;

/// The only tools a read-only built-in agent has, sorted.
const READ_ONLY: [&str; 7] = [
    "code_outline",
    "code_read_range",
    "code_search",
    "code_symbol",
    "grep",
    "read",
    "retrieve",
];
const NAMES: [&str; 4] = ["explore", "implementer", "reviewer", "tester"];

fn agent(name: &str, tools: &str, body: &str) -> String {
    format!(
        "+++\nname = \"{name}\"\n\
         description = \"Does {name}. Use when asked. Do not use otherwise.\"\n\
         tier = \"fast\"\neffort = \"low\"\ntools = [{tools}]\n+++\n{body}\n"
    )
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A catalog over a user directory and a repository holding `project_reviewer`,
/// with the repository trusted or not.
fn with_project_reviewer(trusted: bool, project_reviewer: &str) -> Catalog {
    let (user, repo, data) = (
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    );
    write(repo.path(), ".coda/agents/reviewer.md", project_reviewer);
    let mut store = Store::open(&data.path().join("data")).unwrap();
    if trusted {
        trust_project(&mut store, repo.path()).unwrap();
    }
    Catalog::load_trusted(&store, user.path(), Some(repo.path())).unwrap()
}

#[test]
fn the_built_in_agents_load_with_no_files_and_pass_the_lint_checks() {
    // Arrange
    let user = TempDir::new().unwrap();

    // Act
    let catalog = Catalog::load(user.path(), None);
    let lint = Catalog::check_all(user.path(), None);

    // Assert
    let names: Vec<&str> = catalog.agents().map(|e| e.def.name.as_str()).collect();
    assert_eq!(names, NAMES);
    assert!(catalog.agents().all(|e| e.source == Source::Builtin));
    assert_eq!(lint, []);
}

#[test]
fn explore_and_reviewer_have_exactly_the_read_only_tools() {
    // Arrange
    let catalog = Catalog::load(TempDir::new().unwrap().path(), None);
    let sorted = |name: &str| {
        let mut tools = catalog.agent(name).unwrap().def.tools.clone();
        tools.sort();
        tools
    };

    // Act
    let (explore, reviewer) = (sorted("explore"), sorted("reviewer"));

    // Assert
    assert_eq!(explore, READ_ONLY);
    assert_eq!(reviewer, READ_ONLY);
}

#[test]
fn the_implementer_can_edit_and_run_and_the_tester_can_run_but_not_edit_and_says_not_to_change_files()
 {
    // Arrange
    let catalog = Catalog::load(TempDir::new().unwrap().path(), None);
    let def = |name: &str| &catalog.agent(name).unwrap().def;

    // Act
    let (implementer, tester) = (def("implementer"), def("tester"));

    // Assert
    for tool in ["bash", "edit"] {
        assert!(implementer.tools.iter().any(|t| t == tool), "{tool}");
    }
    assert!(implementer.body.contains("`bash`"));
    assert!(tester.tools.iter().any(|t| t == "bash"));
    assert!(!tester.tools.iter().any(|t| t == "edit"));
    assert!(
        tester.body.contains("do not change files"),
        "{}",
        tester.body
    );
    assert!(tester.body.contains("permission"), "{}", tester.body);
}

#[test]
fn the_json_example_in_the_reviewer_prompt_is_a_report_the_parser_accepts() {
    // Arrange
    let catalog = Catalog::load(TempDir::new().unwrap().path(), None);
    let body = &catalog.agent("reviewer").unwrap().def.body;
    let example = body
        .lines()
        .find(|l| l.starts_with("{\"summary\""))
        .unwrap();

    // Act
    let report = parse_report(example).unwrap();

    // Assert
    let severities: Vec<Severity> = report.findings.iter().map(|f| f.severity).collect();
    assert_eq!(
        severities,
        [
            Severity::Blocking,
            Severity::NonBlocking,
            Severity::Suggestion
        ]
    );
    assert!(report.findings.iter().all(|f| !f.message.is_empty()));
    assert_eq!(report.findings[0].location.as_deref(), Some("src/a.rs:10"));
    assert_eq!(report.findings[2].location, None);
}

#[test]
fn a_user_file_replaces_a_built_in_by_name_and_may_give_it_more_tools() {
    // Arrange
    let user = TempDir::new().unwrap();
    write(
        user.path(),
        "agents/reviewer.md",
        &agent("reviewer", "\"read\", \"bash\"", "Mine."),
    );

    // Act
    let catalog = Catalog::load(user.path(), None);

    // Assert
    let reviewer = catalog.agent("reviewer").unwrap();
    assert_eq!(reviewer.source, Source::User);
    assert_eq!(reviewer.def.tools, ["read", "bash"]);
    assert_eq!(reviewer.def.body.trim(), "Mine.");
}

#[test]
fn a_trusted_project_file_may_narrow_a_built_in_but_not_widen_it() {
    // Arrange
    let project = agent("reviewer", "\"read\", \"bash\"", "Project prompt.");

    // Act
    let catalog = with_project_reviewer(true, &project);

    // Assert
    let reviewer = catalog.agent("reviewer").unwrap();
    assert_eq!(reviewer.source, Source::Project);
    assert_eq!(
        reviewer.def.tier,
        Tier::Standard,
        "the built-in's tier stays"
    );
    assert_eq!(
        reviewer.def.effort,
        Effort::Medium,
        "the built-in's effort stays"
    );
    assert_eq!(
        reviewer.def.tools,
        ["read"],
        "bash is not a tool the built-in has"
    );
    assert_eq!(reviewer.def.body.trim(), "Project prompt.");
    assert!(
        catalog
            .problems()
            .iter()
            .any(|p| p.to_string().contains("bash"))
    );
}

#[test]
fn an_untrusted_project_file_leaves_the_built_in_alone() {
    // Arrange
    let project = agent("reviewer", "\"read\"", "Project prompt.");

    // Act
    let catalog = with_project_reviewer(false, &project);

    // Assert
    let reviewer = catalog.agent("reviewer").unwrap();
    assert_eq!(reviewer.source, Source::Builtin);
    assert_ne!(reviewer.def.body.trim(), "Project prompt.");
    assert_eq!(catalog.pending().len(), 1);
}

/// A project `reviewer` file with the given tier and effort.
fn reviewer_file(tier: &str, effort: &str) -> String {
    format!(
        "+++\nname = \"reviewer\"\n\
         description = \"Does reviewer. Use when asked. Do not use otherwise.\"\n\
         tier = \"{tier}\"\neffort = \"{effort}\"\ntools = [\"read\"]\n+++\nProject prompt.\n"
    )
}

#[test]
fn a_project_file_with_another_tier_or_effort_than_the_built_in_is_reported_and_ignored() {
    // Arrange
    let project = reviewer_file("deep", "high");

    // Act
    let catalog = with_project_reviewer(true, &project);

    // Assert
    let reviewer = catalog.agent("reviewer").unwrap();
    assert_eq!(reviewer.def.tier, Tier::Standard);
    assert_eq!(reviewer.def.effort, Effort::Medium);
    let problems: Vec<(String, &str)> = catalog
        .problems()
        .iter()
        .map(|p| {
            let file = p.path.file_name().unwrap().to_string_lossy().into_owned();
            (file, p.field.as_str())
        })
        .collect();
    assert_eq!(
        problems,
        [
            ("reviewer.md".to_owned(), "tier"),
            ("reviewer.md".to_owned(), "effort")
        ]
    );
    let message = catalog.problems()[0].to_string();
    assert!(
        message.contains("deep") && message.contains("standard"),
        "{message}"
    );
}

#[test]
fn a_project_file_with_the_same_tier_and_effort_as_the_built_in_reports_nothing() {
    // Arrange
    let project = reviewer_file("standard", "medium");

    // Act
    let catalog = with_project_reviewer(true, &project);

    // Assert
    assert_eq!(catalog.problems(), []);
}

#[test]
fn a_held_back_project_file_is_reported_as_overriding_the_built_in_it_is_named_after() {
    // Arrange
    let project = agent("reviewer", "\"read\"", "Project prompt.");
    let (held, used) = (
        with_project_reviewer(false, &project),
        with_project_reviewer(true, &project),
    );

    // Act
    let overriding = held.pending_override("reviewer");

    // Assert
    let pending = overriding.unwrap();
    assert!(pending.path.ends_with(".coda/agents/reviewer.md"));
    assert_eq!(pending.reason, PendingReason::NotTrusted);
    assert_eq!(held.pending_override("explore"), None);
    assert_eq!(held.pending_override("nope"), None);
    assert_eq!(used.pending_override("reviewer"), None, "it is in use now");
}

#[test]
fn a_held_back_project_agent_with_no_agent_of_that_name_overrides_nothing() {
    // Arrange
    let repo = TempDir::new().unwrap();
    write(
        repo.path(),
        ".coda/agents/local.md",
        &agent("local", "\"read\"", "p"),
    );

    // Act
    let catalog = Catalog::load(TempDir::new().unwrap().path(), Some(repo.path()));

    // Assert
    assert_eq!(catalog.pending().len(), 1);
    assert_eq!(catalog.pending_override("local"), None);
}
