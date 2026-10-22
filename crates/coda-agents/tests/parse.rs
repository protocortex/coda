// SPDX-License-Identifier: AGPL-3.0-only

//! The three definition formats, with good and bad files.

use coda_agents::{Effort, Issue, Tier, parse_agent, parse_command, parse_skill};

const DESCRIPTION: &str =
    "Reviews a diff. Use when a change needs review. Do not use when code must be written.";

fn agent_text(extra: &str) -> String {
    format!(
        "+++\nname = \"reviewer\"\ndescription = \"{DESCRIPTION}\"\ntier = \"standard\"\n\
         effort = \"high\"\ntools = [\"read\", \"grep\"]\n{extra}+++\nYou review diffs.\n"
    )
}

fn fields(result: Result<impl std::fmt::Debug, Vec<Issue>>) -> Vec<String> {
    result.unwrap_err().into_iter().map(|i| i.field).collect()
}

#[test]
fn a_good_agent_parses_into_typed_fields() {
    // Arrange
    let text = agent_text("");

    // Act
    let agent = parse_agent(&text, "reviewer").unwrap();

    // Assert
    assert_eq!(agent.name, "reviewer");
    assert_eq!(agent.tier, Tier::Standard);
    assert_eq!(agent.effort, Effort::High);
    assert_eq!(agent.tools, ["read", "grep"]);
    assert_eq!(agent.body, "You review diffs.\n");
}

#[test]
fn an_unknown_frontmatter_key_is_rejected() {
    // Arrange
    let text = agent_text("colour = \"red\"\n");

    // Act
    let issues = parse_agent(&text, "reviewer").unwrap_err();

    // Assert
    assert!(
        issues.iter().any(|i| i.message.contains("colour")),
        "{issues:?}"
    );
}

#[test]
fn a_missing_field_is_rejected() {
    // Arrange
    let text = agent_text("").replace("tier = \"standard\"\n", "");

    // Act
    let issues = parse_agent(&text, "reviewer").unwrap_err();

    // Assert
    assert!(
        issues.iter().any(|i| i.message.contains("tier")),
        "{issues:?}"
    );
}

#[test]
fn a_model_id_in_an_agent_file_is_rejected() {
    // Arrange
    let text = agent_text("model = \"claude-sonnet-4-5\"\n");

    // Act
    let result = parse_agent(&text, "reviewer");

    // Assert
    assert_eq!(fields(result), ["model"]);
}

#[test]
fn a_bad_tier_and_effort_are_each_named() {
    // Arrange
    let text = agent_text("")
        .replace("\"standard\"", "\"claude-opus\"")
        .replace("\"high\"", "\"extreme\"");

    // Act
    let result = parse_agent(&text, "reviewer");

    // Assert
    assert_eq!(fields(result), ["tier", "effort"]);
}

#[test]
fn the_name_must_match_the_file_name() {
    // Arrange
    let text = agent_text("");

    // Act
    let result = parse_agent(&text, "other");

    // Assert
    assert_eq!(fields(result), ["name"]);
}

#[test]
fn text_without_frontmatter_is_rejected() {
    // Act
    let result = parse_agent("just a prompt", "reviewer");

    // Assert
    assert_eq!(fields(result), ["frontmatter"]);
}

#[test]
fn hook_keys_are_rejected_in_every_format() {
    // Arrange
    let agent = agent_text("hooks = [\"x\"]\n");
    let skill = "+++\nname = \"s\"\ndescription = \"d\"\npre_tool = \"x\"\n+++\nbody";
    let command = "+++\nname = \"c\"\ndescription = \"d\"\nhook = \"x\"\n+++\nbody";

    // Act and Assert
    assert_eq!(fields(parse_agent(&agent, "reviewer")), ["hooks"]);
    assert_eq!(fields(parse_skill(skill, "s")), ["pre_tool"]);
    assert_eq!(fields(parse_command(command, "c")), ["hook"]);
}

#[test]
fn skills_and_commands_parse_with_an_optional_argument_hint() {
    // Arrange
    let skill = "+++\nname = \"tdd\"\ndescription = \"Write tests first\"\n+++\nSteps";
    let command =
        "+++\nname = \"fix\"\ndescription = \"Fix it\"\nargument_hint = \"<issue>\"\n+++\nDo it";

    // Act
    let skill = parse_skill(skill, "tdd").unwrap();
    let command = parse_command(command, "fix").unwrap();

    // Assert
    assert_eq!(skill.body, "Steps");
    assert_eq!(command.argument_hint.as_deref(), Some("<issue>"));
}
