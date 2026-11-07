// SPDX-License-Identifier: Apache-2.0

//! The context a person adds to `coda review`: files of test, coverage or linter output, and
//! the output of the configured checks run by the `tester` agent first.

use coda_config::review::{CHECK_COMMANDS_KEY, REVIEW_KEY, ReviewConfig};
use coda_protocol::types::BackendKind;
use coda_review::context::{ContextFile, load};

use crate::cli::ReviewArgs;
use crate::error::CliError;
use crate::review_route::Attempt;

/// What the block of the checks' output is called.
const CHECKS_NAME: &str = "checks run by the tester agent";

/// What the person asked to add to the prompt, checked before any model runs.
#[derive(Debug)]
pub(crate) struct Asked {
    /// The context files, read.
    pub(crate) files: Vec<ContextFile>,
    /// The commands the tester runs first, when `--run-checks` is given.
    pub(crate) commands: Option<Vec<String>>,
}

impl Asked {
    /// Reads the context files and checks that `--run-checks` can run.
    ///
    /// # Errors
    /// Returns [`CliError::ReviewInput`] when a file cannot be read or there are too many,
    /// when `--run-checks` is given with no `[review] check_commands`, and when it is given on
    /// a backend whose review session is read-only.
    pub(crate) fn new(
        args: &ReviewArgs,
        config: &ReviewConfig,
        backend: BackendKind,
    ) -> Result<Self, CliError> {
        let files = load(&args.context_file).map_err(|e| CliError::ReviewInput(e.to_string()))?;
        let commands = args
            .run_checks
            .then(|| checks_to_run(config, backend))
            .transpose()?;
        Ok(Self { files, commands })
    }
}

fn checks_to_run(config: &ReviewConfig, backend: BackendKind) -> Result<Vec<String>, CliError> {
    if config.check_commands.is_empty() {
        return Err(CliError::ReviewInput(format!(
            "--run-checks needs commands to run: set `{CHECK_COMMANDS_KEY}` in the [{REVIEW_KEY}] \
             table of the config"
        )));
    }
    if !backend.is_model_backend() {
        return Err(CliError::ReviewInput(
            "--run-checks needs a model-API backend (anthropic or openai_compat): a review on \
             Claude Code or Codex is read-only and denies every command, so the tester could \
             run nothing"
                .to_owned(),
        ));
    }
    Ok(config.check_commands.clone())
}

/// What the tester run gave.
#[derive(Debug)]
pub(crate) struct Checks {
    /// The block for the prompt: the tester's report, or a line saying it did not run.
    pub(crate) file: ContextFile,
    /// Whether the tester gave a report.
    pub(crate) ran: bool,
}

/// Has the tester run the checks. A tester that fails does not stop the review: its block
/// says the checks did not run, so the reviewer does not read their absence as a pass. Only
/// an interrupt ends the review.
///
/// # Errors
/// Returns [`CliError::RunInterrupted`] when the user cancels.
pub(crate) async fn run_checks(
    tester: &mut impl Attempt,
    backend: BackendKind,
) -> Result<Checks, CliError> {
    match tester.attempt(backend, None, None).await {
        Ok(answer) if !answer.text.trim().is_empty() => Ok(Checks {
            file: ContextFile::new(CHECKS_NAME, &answer.text, false),
            ran: true,
        }),
        Ok(_) => Ok(not_run("the tester gave no report")),
        Err(CliError::RunInterrupted) => Err(CliError::RunInterrupted),
        Err(error) => Ok(not_run(&error.to_string())),
    }
}

fn not_run(why: &str) -> Checks {
    Checks {
        file: ContextFile::new(
            CHECKS_NAME,
            &format!("The checks did not run: {why}. Their absence says nothing about the code."),
            false,
        ),
        ran: false,
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::*;
    use crate::review::Answer;

    fn config(commands: &[&str]) -> ReviewConfig {
        ReviewConfig {
            check_commands: commands.iter().map(|c| (*c).to_owned()).collect(),
            ..ReviewConfig::default()
        }
    }

    #[test]
    fn checks_need_commands_and_a_model_api_backend() {
        // Act
        let none = checks_to_run(&config(&[]), BackendKind::Anthropic).unwrap_err();
        let delegated =
            checks_to_run(&config(&["cargo test"]), BackendKind::ClaudeCode).unwrap_err();
        let fine = checks_to_run(
            &config(&["cargo test", "cargo clippy"]),
            BackendKind::Anthropic,
        );

        // Assert
        assert!(none.to_string().contains("check_commands"), "{none}");
        assert!(delegated.to_string().contains("read-only"), "{delegated}");
        assert_eq!(fine.unwrap(), ["cargo test", "cargo clippy"]);
    }

    struct Tester(Option<Result<&'static str, CliError>>);

    impl Attempt for Tester {
        fn attempt(
            &mut self,
            _: BackendKind,
            _: Option<&str>,
            _: Option<String>,
        ) -> impl Future<Output = Result<Answer, CliError>> {
            std::future::ready(self.0.take().unwrap().map(|text| Answer {
                text: text.to_owned(),
                model: None,
            }))
        }
    }

    #[tokio::test]
    async fn the_testers_report_becomes_a_block_and_a_failed_tester_becomes_a_note() {
        // Act
        let good = run_checks(
            &mut Tester(Some(Ok("cargo test: 3 passed"))),
            BackendKind::Anthropic,
        )
        .await
        .unwrap();
        let empty = run_checks(&mut Tester(Some(Ok("  "))), BackendKind::Anthropic)
            .await
            .unwrap();
        let failed = run_checks(
            &mut Tester(Some(Err(CliError::ReviewFailed("down".to_owned())))),
            BackendKind::Anthropic,
        )
        .await
        .unwrap();

        // Assert
        assert!(good.ran);
        assert_eq!(good.file.text, "cargo test: 3 passed");
        assert!(!empty.ran);
        assert!(empty.file.text.starts_with("The checks did not run"));
        assert!(!failed.ran);
        assert!(failed.file.text.contains("down"), "{}", failed.file.text);
    }

    #[tokio::test]
    async fn an_interrupt_ends_the_review_instead_of_becoming_a_note() {
        // Act
        let result = run_checks(
            &mut Tester(Some(Err(CliError::RunInterrupted))),
            BackendKind::Anthropic,
        )
        .await;

        // Assert
        assert!(matches!(result, Err(CliError::RunInterrupted)));
    }
}
