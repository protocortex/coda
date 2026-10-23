// SPDX-License-Identifier: AGPL-3.0-only

//! `coda run --command`: a command file becomes the prompt of a headless run.

mod common;

use std::fs;
use std::io::{self, BufRead, Read};
use std::path::Path;
use std::process::ExitCode;

use coda_agents::trust_project;
use coda_cli::MAX_COMMAND_INPUT_BYTES;
use coda_config::config::Dirs;
use coda_protocol::paths::repo_identity;
use coda_store::Store;
use common::{FauxRig as Rig, Outcome, cli, cli_reading, faux_rig};
use serde_json::{Value, json};

const REVIEW: &str = "+++\nname = \"review\"\ndescription = \"Reviews a change.\"\n+++\n\
    Review $ARGUMENTS and report problems only.\n";
const ASK: &str = "+++\nname = \"ask\"\ndescription = \"Asks.\"\n+++\n$ARGUMENTS\n";
const STATUS: &str = "+++\nname = \"status\"\ndescription = \"Summarizes.\"\n+++\n\
    Summarize the repository.\n";
const DEPLOY: &str =
    "+++\nname = \"deploy\"\ndescription = \"Ships it.\"\n+++\nDeploy $ARGUMENTS\n";

/// A rig whose faux provider answers once.
fn rig() -> Rig {
    let answer = json!({"events": [
        {"type": "text_delta", "text": "all done"},
        {"type": "stop", "reason": "end_turn"},
    ]});
    faux_rig(&[answer])
}

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

impl Rig {
    async fn run(&self, flags: &[&str]) -> Outcome {
        self.run_with_input(flags, "").await
    }

    /// Runs with `input` on standard input.
    async fn run_with_input(&self, flags: &[&str], input: &str) -> Outcome {
        let cwd = self.work.path().display().to_string();
        let mut args = vec!["coda", "run", "--cwd", &cwd];
        args.extend_from_slice(flags);
        cli(&self.ctx, &args, input).await
    }

    /// Runs reading standard input from `input`.
    async fn run_reading(&self, flags: &[&str], input: &mut dyn BufRead) -> Outcome {
        let cwd = self.work.path().display().to_string();
        let mut args = vec!["coda", "run", "--cwd", &cwd];
        args.extend_from_slice(flags);
        cli_reading(&self.ctx, &args, input).await
    }
}

#[tokio::test]
async fn a_command_file_with_arguments_becomes_the_prompt_of_the_run() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/review.md", REVIEW);

    // Act
    let outcome = rig
        .run(&["--command", "review", "--args", "PR 42 $ARGUMENTS"])
        .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(outcome.out, "all done\n");
    let requests = rig.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0]["messages"][0],
        json!({
            "role": "user",
            "text": "Review PR 42 $ARGUMENTS and report problems only."
        })
    );
}

#[tokio::test]
async fn an_unknown_command_fails_before_any_model_call() {
    // Arrange
    let rig = rig();

    // Act
    let outcome = rig.run(&["--command", "nope"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(70), "{}", outcome.err);
    assert!(
        outcome.err.contains("no command named nope"),
        "{}",
        outcome.err
    );
    assert!(
        !outcome.err.contains("coda agents lint"),
        "no file was skipped, so there is nothing to lint: {}",
        outcome.err
    );
    assert_eq!(rig.requests(), Vec::<Value>::new());
}

#[tokio::test]
async fn an_unknown_command_points_at_lint_when_a_definition_file_was_skipped() {
    // Arrange
    let rig = rig();
    write(
        rig.home.path(),
        "commands/review.md",
        "no frontmatter at all",
    );

    // Act
    let outcome = rig.run(&["--command", "review"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(70), "{}", outcome.err);
    assert!(
        outcome.err.contains("no command named review"),
        "{}",
        outcome.err
    );
    assert!(
        outcome
            .err
            .contains("1 definition file was skipped or only partly used; run `coda agents lint`"),
        "{}",
        outcome.err
    );
    assert_eq!(rig.requests(), Vec::<Value>::new());
}

#[tokio::test]
async fn a_command_that_expands_to_a_blank_prompt_fails_before_any_model_call() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/ask.md", ASK);

    // Act
    let outcome = rig.run(&["--command", "ask"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(70), "{}", outcome.err);
    assert!(
        outcome.err.contains("ask: the command's prompt is blank"),
        "{}",
        outcome.err
    );
    assert_eq!(rig.requests(), Vec::<Value>::new());
}

#[tokio::test]
async fn piped_standard_input_is_the_arguments_when_there_is_no_args_flag() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/review.md", REVIEW);

    // Act
    let outcome = rig
        .run_with_input(&["--command", "review"], "the staged diff\n")
        .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(
        rig.requests()[0]["messages"][0],
        json!({
            "role": "user",
            "text": "Review the staged diff and report problems only."
        })
    );
}

#[tokio::test]
async fn the_args_flag_wins_over_piped_standard_input() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/review.md", REVIEW);

    // Act
    let outcome = rig
        .run_with_input(&["--command", "review", "--args", "PR 42"], "ignored\n")
        .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(
        rig.requests()[0]["messages"][0]["text"],
        "Review PR 42 and report problems only."
    );
}

/// Standard input that remembers whether anyone read it, and holds nothing.
#[derive(Default)]
struct Watched {
    touched: bool,
}

impl Read for Watched {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        self.touched = true;
        Ok(0)
    }
}

impl BufRead for Watched {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.touched = true;
        Ok(&[])
    }

    fn consume(&mut self, _amount: usize) {}
}

#[tokio::test]
async fn a_command_without_a_placeholder_never_reads_standard_input() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/status.md", STATUS);
    let mut input = Watched::default();

    // Act
    let outcome = rig.run_reading(&["--command", "status"], &mut input).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert!(!input.touched, "standard input was read");
    assert_eq!(
        rig.requests()[0]["messages"][0]["text"],
        "Summarize the repository."
    );
}

#[tokio::test]
async fn unrelated_piped_input_does_not_fail_a_command_without_a_placeholder() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/status.md", STATUS);

    // Act
    let outcome = rig
        .run_with_input(&["--command", "status"], "a diff\n")
        .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(
        rig.requests()[0]["messages"][0]["text"],
        "Summarize the repository."
    );
}

#[tokio::test]
async fn an_unknown_or_pending_command_fails_before_standard_input_is_read() {
    for name in ["nope", "deploy"] {
        // Arrange: `deploy` is a project file that waits for trust.
        let rig = rig();
        write(rig.work.path(), ".coda/commands/deploy.md", DEPLOY);
        let mut input = Watched::default();

        // Act
        let outcome = rig.run_reading(&["--command", name], &mut input).await;

        // Assert
        assert_eq!(outcome.code, ExitCode::from(70), "{name}: {}", outcome.err);
        assert!(!input.touched, "{name}: standard input was read");
        assert_eq!(rig.requests(), Vec::<Value>::new(), "{name}");
    }
}

#[tokio::test]
async fn a_terminal_on_standard_input_is_never_read_for_the_arguments() {
    // Arrange: input is on offer, but standard input is a terminal.
    let mut rig = rig();
    rig.ctx.stdin_is_tty = true;
    write(rig.home.path(), "commands/review.md", REVIEW);

    // Act
    let outcome = rig
        .run_with_input(&["--command", "review"], "would be the arguments\n")
        .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    assert_eq!(
        rig.requests()[0]["messages"][0]["text"],
        "Review  and report problems only."
    );
}

#[tokio::test]
async fn standard_input_over_the_cap_is_refused_before_any_model_call() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/review.md", REVIEW);
    let too_big = "x".repeat(MAX_COMMAND_INPUT_BYTES + 1);

    // Act
    let outcome = rig.run_with_input(&["--command", "review"], &too_big).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(70), "{}", outcome.err);
    assert!(
        outcome
            .err
            .contains(&format!("more than {MAX_COMMAND_INPUT_BYTES} bytes")),
        "{}",
        outcome.err
    );
    assert_eq!(rig.requests(), Vec::<Value>::new());
}

#[tokio::test]
async fn standard_input_exactly_at_the_cap_is_the_arguments() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/review.md", REVIEW);
    let full = "x".repeat(MAX_COMMAND_INPUT_BYTES);

    // Act
    let outcome = rig.run_with_input(&["--command", "review"], &full).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::SUCCESS, "{}", outcome.err);
    let text = rig.requests()[0]["messages"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(text, format!("Review {full} and report problems only."));
}

#[tokio::test]
async fn arguments_for_a_command_without_a_placeholder_fail_before_any_model_call() {
    // Arrange
    let rig = rig();
    write(rig.home.path(), "commands/status.md", STATUS);

    // Act
    let outcome = rig
        .run(&["--command", "status", "--args", "extra words"])
        .await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(70), "{}", outcome.err);
    assert!(
        outcome.err.contains("takes no arguments"),
        "{}",
        outcome.err
    );
    assert_eq!(rig.requests(), Vec::<Value>::new());
}

#[tokio::test]
async fn a_pending_project_command_does_not_run_until_the_repository_is_trusted() {
    // Arrange
    let rig = rig();
    write(rig.work.path(), ".coda/commands/deploy.md", DEPLOY);

    // Act
    let pending = rig.run(&["--command", "deploy", "--args", "staging"]).await;
    let held_back = rig.requests();
    let mut store = Store::open(&Dirs::from_base(rig.home.path()).data_dir()).unwrap();
    let root = repo_identity(rig.work.path()).unwrap();
    trust_project(&mut store, &root).unwrap();
    drop(store);
    let trusted = rig.run(&["--command", "deploy", "--args", "staging"]).await;

    // Assert
    assert_eq!(pending.code, ExitCode::from(70), "{}", pending.err);
    assert!(pending.err.contains("not in use"), "{}", pending.err);
    assert!(pending.err.contains("coda agents trust"), "{}", pending.err);
    assert_eq!(held_back, Vec::<Value>::new());
    assert_eq!(trusted.code, ExitCode::SUCCESS, "{}", trusted.err);
    assert_eq!(
        rig.requests()[0]["messages"][0],
        json!({"role": "user", "text": "Deploy staging"})
    );
}

#[tokio::test]
async fn a_name_that_only_looks_like_a_command_file_is_refused_before_any_model_call() {
    for name in ["../x", "Deploy", "deploy/"] {
        // Arrange
        let rig = rig();
        write(rig.home.path(), "commands/deploy.md", DEPLOY);
        // What a path trick would reach: a file next to the commands directory.
        write(rig.home.path(), "x.md", DEPLOY);

        // Act
        let outcome = rig.run(&["--command", name, "--args", "now"]).await;

        // Assert
        assert_eq!(outcome.code, ExitCode::from(70), "{name}: {}", outcome.err);
        assert!(
            outcome.err.contains(&format!("no command named {name}")),
            "{name}: {}",
            outcome.err
        );
        assert_eq!(rig.requests(), Vec::<Value>::new(), "{name}");
    }
}

#[tokio::test]
async fn a_command_and_a_prompt_together_are_a_usage_error() {
    // Arrange
    let rig = rig();

    // Act
    let outcome = rig.run(&["--command", "review", "--prompt", "hello"]).await;

    // Assert
    assert_eq!(outcome.code, ExitCode::from(64), "{}", outcome.err);
    assert_eq!(rig.requests(), Vec::<Value>::new());
}
