// SPDX-License-Identifier: Apache-2.0

//! Moves a part of a review to the other backend when its own is out of quota or gone, for
//! `coda review`.

use coda_config::config::Config;
use coda_protocol::events::InterruptCause;
use coda_protocol::types::BackendKind;

use crate::error::{CliError, Unavailable};
use crate::review::{Answer, Reviewer, backend_name};
use crate::run::{CLAUDE_CODE_KEY, CODEX_KEY, Finish};

/// Something that can run an agent once on a chosen backend and model.
pub(crate) trait Attempt {
    async fn attempt(
        &mut self,
        backend: BackendKind,
        model: Option<&str>,
        hint: Option<String>,
    ) -> Result<Answer, CliError>;
}

/// Runs one part on its backend, and on the next backend of the list when that one hits a
/// usage limit or is lost. A backend that was left stays left for the rest of the part.
pub(crate) struct Failover<A> {
    attempt: A,
    backends: Vec<BackendKind>,
    model: Option<String>,
    label: String,
    active: usize,
    notes: Vec<String>,
}

impl<A: Attempt> Failover<A> {
    /// `backends` is the order to try, the one asked for first, and is not empty. `model`
    /// applies to the first only, since a model name belongs to one backend. `label` names the
    /// part in the notes.
    pub(crate) fn new(
        attempt: A,
        backends: Vec<BackendKind>,
        model: Option<String>,
        label: String,
    ) -> Self {
        Self {
            attempt,
            backends,
            model,
            label,
            active: 0,
            notes: Vec::new(),
        }
    }

    /// One line for each time the part moved to another backend.
    pub(crate) fn notes(&self) -> &[String] {
        &self.notes
    }
}

impl<A: Attempt> Reviewer for Failover<A> {
    async fn run(&mut self, hint: Option<String>) -> Result<Answer, CliError> {
        loop {
            let backend = self.backends[self.active];
            let model = if self.active == 0 {
                self.model.as_deref()
            } else {
                None
            };
            match self.attempt.attempt(backend, model, hint.clone()).await {
                Err(CliError::BackendUnavailable(reason)) => {
                    let Some(next) = self.backends.get(self.active + 1) else {
                        return Err(CliError::BackendUnavailable(reason));
                    };
                    self.notes.push(format!(
                        "{}: {} {reason}, so it ran on {}",
                        self.label,
                        backend_name(backend),
                        backend_name(*next),
                    ));
                    self.active += 1;
                }
                done => return done,
            }
        }
    }
}

/// The other of Claude Code and Codex, when it is enabled in the config. Coda has no check
/// of whether you are logged in, so enabled stands for it.
pub(crate) fn other_backend(config: &Config, backend: BackendKind) -> Option<BackendKind> {
    let enabled = |key: &str| config.providers.get(key).is_some_and(|p| p.enabled);
    match backend {
        BackendKind::ClaudeCode if enabled(CODEX_KEY) => Some(BackendKind::CodexAppServer),
        BackendKind::CodexAppServer | BackendKind::CodexExec if enabled(CLAUDE_CODE_KEY) => {
            Some(BackendKind::ClaudeCode)
        }
        _ => None,
    }
}

/// Why a run that did not finish says its backend cannot go on, when it does: a usage limit
/// ended the turn, or the backend process went away.
pub(crate) fn unavailable_reason(limit_hit: bool, finish: &Finish) -> Option<Unavailable> {
    if limit_hit {
        return Some(Unavailable::UsageLimit);
    }
    let lost = finish.cut_short && finish.cause == Some(InterruptCause::BackendLost);
    lost.then_some(Unavailable::Lost)
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::*;

    const CLAUDE: BackendKind = BackendKind::ClaudeCode;
    const CODEX: BackendKind = BackendKind::CodexAppServer;

    /// Answers by backend: `limited` ones fail as unavailable, the rest give their own name.
    struct Fake {
        limited: Vec<BackendKind>,
        seen: Vec<(BackendKind, Option<String>)>,
    }

    impl Attempt for Fake {
        fn attempt(
            &mut self,
            backend: BackendKind,
            model: Option<&str>,
            _hint: Option<String>,
        ) -> impl Future<Output = Result<Answer, CliError>> {
            self.seen.push((backend, model.map(str::to_owned)));
            std::future::ready(if self.limited.contains(&backend) {
                Err(CliError::BackendUnavailable(Unavailable::UsageLimit))
            } else {
                Ok(Answer {
                    text: backend_name(backend),
                    model: model.map(str::to_owned),
                })
            })
        }
    }

    fn fake(limited: &[BackendKind]) -> Fake {
        Fake {
            limited: limited.to_vec(),
            seen: Vec::new(),
        }
    }

    fn config(toml: &str) -> Config {
        Config::from_toml_str(toml).unwrap().config
    }

    fn finish(cut_short: bool, cause: Option<InterruptCause>) -> Finish {
        Finish {
            condition: None,
            failed: true,
            cut_short,
            cause,
            hook_reason: None,
        }
    }

    #[tokio::test]
    async fn a_chunk_that_hits_a_limit_runs_on_the_second_backend_and_says_so() {
        // Arrange
        let mut part = Failover::new(
            fake(&[CLAUDE]),
            vec![CLAUDE, CODEX],
            Some("cheap".to_owned()),
            "part 2 of 3".to_owned(),
        );

        // Act
        let answer = part.run(None).await.unwrap();

        // Assert
        assert_eq!(answer.text, "codex_app_server");
        assert_eq!(
            part.attempt.seen,
            [(CLAUDE, Some("cheap".to_owned())), (CODEX, None)]
        );
        assert_eq!(
            part.notes(),
            ["part 2 of 3: claude_code hit a usage limit, so it ran on codex_app_server"]
        );
    }

    #[tokio::test]
    async fn the_backend_that_was_left_is_not_asked_again_by_the_next_run() {
        // Arrange
        let mut part = Failover::new(fake(&[CLAUDE]), vec![CLAUDE, CODEX], None, String::new());

        // Act
        part.run(None).await.unwrap();
        part.run(Some("again".to_owned())).await.unwrap();

        // Assert
        assert_eq!(part.attempt.seen.len(), 3);
        assert_eq!(part.attempt.seen[2].0, CODEX);
        assert_eq!(part.notes().len(), 1);
    }

    #[tokio::test]
    async fn with_one_backend_the_unavailable_error_comes_out_and_nothing_is_dropped() {
        // Arrange
        let mut part = Failover::new(fake(&[CLAUDE]), vec![CLAUDE], None, String::new());

        // Act
        let error = part.run(None).await.unwrap_err();

        // Assert
        assert!(matches!(error, CliError::BackendUnavailable(_)), "{error}");
        assert_eq!(part.notes(), &[] as &[String]);
    }

    #[tokio::test]
    async fn when_both_backends_are_out_the_second_error_comes_out() {
        // Arrange
        let mut part = Failover::new(
            fake(&[CLAUDE, CODEX]),
            vec![CLAUDE, CODEX],
            None,
            "part 1 of 1".to_owned(),
        );

        // Act
        let error = part.run(None).await.unwrap_err();

        // Assert
        assert!(matches!(error, CliError::BackendUnavailable(_)), "{error}");
        assert_eq!(part.attempt.seen.len(), 2);
    }

    #[tokio::test]
    async fn an_error_that_is_not_about_the_backend_does_not_move_the_part() {
        // Arrange
        struct Broken;
        impl Attempt for Broken {
            fn attempt(
                &mut self,
                _: BackendKind,
                _: Option<&str>,
                _: Option<String>,
            ) -> impl Future<Output = Result<Answer, CliError>> {
                std::future::ready(Err(CliError::ReviewFailed("bad".to_owned())))
            }
        }
        let mut part = Failover::new(Broken, vec![CLAUDE, CODEX], None, String::new());

        // Act
        let error = part.run(None).await.unwrap_err();

        // Assert
        assert!(matches!(error, CliError::ReviewFailed(_)), "{error}");
        assert_eq!(part.notes(), &[] as &[String]);
    }

    #[test]
    fn the_other_backend_is_the_other_enabled_one() {
        // Arrange
        let both =
            config("[providers.claude_code]\nenabled = true\n[providers.codex]\nenabled = true\n");
        let only_claude = config("[providers.claude_code]\nenabled = true\n");

        // Act and Assert
        assert_eq!(other_backend(&both, CLAUDE), Some(CODEX));
        assert_eq!(other_backend(&both, CODEX), Some(CLAUDE));
        assert_eq!(other_backend(&both, BackendKind::CodexExec), Some(CLAUDE));
        assert_eq!(other_backend(&only_claude, CLAUDE), None);
        assert_eq!(other_backend(&both, BackendKind::Anthropic), None);
    }

    #[test]
    fn a_usage_limit_or_a_lost_backend_makes_it_unavailable_and_other_stops_do_not() {
        // Act and Assert
        assert_eq!(
            unavailable_reason(true, &finish(false, None)),
            Some(Unavailable::UsageLimit)
        );
        assert_eq!(
            unavailable_reason(false, &finish(true, Some(InterruptCause::BackendLost))),
            Some(Unavailable::Lost)
        );
        assert_eq!(
            unavailable_reason(false, &finish(true, Some(InterruptCause::IdleTimeout))),
            None
        );
        assert_eq!(unavailable_reason(false, &finish(false, None)), None);
    }
}
