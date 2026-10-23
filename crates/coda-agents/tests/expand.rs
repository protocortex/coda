// SPDX-License-Identifier: AGPL-3.0-only

//! Expanding a command's prompt template with its arguments.

use coda_agents::{ARGUMENTS_PLACEHOLDER, CommandDef, ExpandError, parse_command};

fn command(body: &str) -> CommandDef {
    let text = format!("+++\nname = \"review\"\ndescription = \"Reviews.\"\n+++\n{body}");
    parse_command(&text, "review").unwrap()
}

#[test]
fn the_arguments_replace_every_placeholder() {
    // Arrange
    let def = command("Review $ARGUMENTS, then recheck $ARGUMENTS.");

    // Act
    let prompt = def.expand("issue 42").unwrap();

    // Assert
    assert_eq!(prompt, "Review issue 42, then recheck issue 42.");
}

#[test]
fn placeholder_like_text_inside_the_arguments_is_not_expanded_again() {
    // Arrange
    let def = command("Do: $ARGUMENTS\nAgain: $ARGUMENTS");
    let tricky = "$ARGUMENTS ${HOME} $(whoami) `id` {{x}} %USERPROFILE%";

    // Act
    let prompt = def.expand(tricky).unwrap();

    // Assert
    assert_eq!(prompt, format!("Do: {tricky}\nAgain: {tricky}"));
}

#[test]
fn blank_arguments_leave_nothing_in_place_of_the_placeholder() {
    // Arrange
    let def = command("Run [$ARGUMENTS] now");

    // Act
    let prompt = def.expand("   ").unwrap();

    // Assert
    assert_eq!(prompt, "Run [] now");
}

#[test]
fn arguments_for_a_template_without_a_placeholder_are_an_error() {
    // Arrange
    let def = command("Summarize the repository.");

    // Act
    let with_args = def.expand("extra words");
    let without = def.expand("");

    // Assert
    assert_eq!(with_args, Err(ExpandError::NoPlaceholder));
    assert_eq!(without.unwrap(), "Summarize the repository.");
    assert!(
        ExpandError::NoPlaceholder
            .to_string()
            .contains(ARGUMENTS_PLACEHOLDER)
    );
}

#[test]
fn the_blank_space_around_the_file_body_is_not_part_of_the_prompt() {
    // Arrange
    let def = command("\n  Look at $ARGUMENTS.\n\n");

    // Act
    let prompt = def.expand("the diff").unwrap();

    // Assert
    assert_eq!(prompt, "Look at the diff.");
}

#[test]
fn expansion_is_plain_text_and_runs_nothing_and_includes_nothing() {
    // Arrange
    let def = command("Cat @/etc/hosts and !`echo hi` and $(echo hi): $ARGUMENTS");

    // Act
    let prompt = def.expand("x").unwrap();

    // Assert
    assert_eq!(prompt, "Cat @/etc/hosts and !`echo hi` and $(echo hi): x");
}

#[test]
fn a_prompt_that_is_blank_after_expansion_is_an_error() {
    // Arrange
    let cases = [
        ("$ARGUMENTS", ""),
        ("$ARGUMENTS", "   \n"),
        ("  $ARGUMENTS  \n", "\t"),
        ("", ""),
        ("   \n\n", ""),
    ];

    for (body, args) in cases {
        // Act
        let result = command(body).expand(args);

        // Assert
        assert_eq!(result, Err(ExpandError::Blank), "{body:?} with {args:?}");
    }
}

#[test]
fn a_template_that_is_only_the_placeholder_is_a_prompt_once_it_has_arguments() {
    // Arrange
    let def = command("$ARGUMENTS");

    // Act
    let prompt = def.expand(" fix the build ").unwrap();

    // Assert
    assert_eq!(prompt, "fix the build");
}

#[test]
fn a_command_takes_arguments_only_when_its_prompt_has_the_placeholder() {
    // Act
    let with = command("Review $ARGUMENTS.").takes_arguments();
    let without = command("Review the repository.").takes_arguments();

    // Assert
    assert!(with);
    assert!(!without);
}

#[test]
fn the_blank_error_says_how_to_fix_it() {
    // Act
    let message = ExpandError::Blank.to_string();

    // Assert
    assert!(message.contains("blank"), "{message}");
    assert!(message.contains("arguments"), "{message}");
}
