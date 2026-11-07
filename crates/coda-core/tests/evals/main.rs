// SPDX-License-Identifier: Apache-2.0

//! Independent-loop evals: small tasks run through coda's own turn loop with the
//! real tools, and the outcome is judged by a command in a temporary workspace.
//!
//! Offline, a scripted model drives each fixture, so the run checks the loop,
//! the tool text and the cost against `baseline.json`. The live run in
//! `live.rs` is ignored by default and needs an explicit flag and a key variable.

mod baseline;
mod fixtures;
mod live;
mod runner;

mod baseline_tests {
    use crate::baseline::{Baseline, Cost, DEFAULT_MARGIN_PERCENT};
    use crate::runner::Outcome;

    fn outcome(name: &'static str, passed: bool, turns: u32, tokens: u64) -> Outcome {
        Outcome {
            name,
            passed,
            turns,
            tokens,
            usage_tokens: 0,
            shrunk_bytes: 0,
            tool_errors: 0,
            fallbacks: 0,
            stop: "end_turn".to_owned(),
            failure: None,
        }
    }

    fn baseline() -> Baseline {
        Baseline {
            margin_percent: DEFAULT_MARGIN_PERCENT,
            fixtures: [(
                "fix".to_owned(),
                Cost {
                    turns: 4,
                    tokens: 1000,
                },
            )]
            .into(),
        }
    }

    #[test]
    fn a_run_at_the_baseline_has_no_regressions() {
        let found = baseline().regressions(&[outcome("fix", true, 4, 1000)]);

        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn tokens_inside_the_margin_pass_and_past_it_fail() {
        let inside = baseline().regressions(&[outcome("fix", true, 4, 1100)]);
        let past = baseline().regressions(&[outcome("fix", true, 4, 1101)]);

        assert!(inside.is_empty(), "{inside:?}");
        assert_eq!(past.len(), 1, "{past:?}");
        assert!(past[0].contains("tokens"));
    }

    #[test]
    fn an_extra_turn_fails() {
        let found = baseline().regressions(&[outcome("fix", true, 5, 1000)]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("turns"));
    }

    #[test]
    fn a_failed_fixture_fails_even_when_cheap() {
        let found = baseline().regressions(&[outcome("fix", false, 1, 10)]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("did not pass"));
    }

    #[test]
    fn a_fixture_missing_from_the_baseline_fails() {
        let found = baseline().regressions(&[outcome("new", true, 1, 10)]);

        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("no baseline"));
    }
}

mod offline {
    use std::path::PathBuf;

    use crate::baseline::Baseline;
    use crate::fixtures::{self, Fixture};
    use crate::runner::{self, Outcome};

    const BASELINE_FILE: &str = "tests/evals/baseline.json";
    const UPDATE_VAR: &str = "CODA_EVAL_UPDATE";
    const REPORT_VAR: &str = "CODA_EVAL_REPORT";

    fn baseline_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(BASELINE_FILE)
    }

    async fn run_all() -> Vec<Outcome> {
        let mut outcomes = Vec::new();
        for fixture in fixtures::all() {
            outcomes.push(runner::run_offline(&fixture).await);
        }
        outcomes
    }

    fn find<'a>(outcomes: &'a [Outcome], name: &str) -> &'a Outcome {
        outcomes.iter().find(|o| o.name == name).unwrap()
    }

    #[tokio::test]
    async fn every_fixture_passes_with_the_scripted_model() {
        let outcomes = run_all().await;

        let failed: Vec<_> = outcomes.iter().filter(|o| !o.passed).collect();
        assert!(failed.is_empty(), "{failed:#?}");
    }

    #[tokio::test]
    async fn a_run_that_changes_nothing_fails_its_check() {
        let idle = Fixture::idle("idle");

        let outcome = runner::run_offline(&idle).await;

        assert!(!outcome.passed, "{outcome:?}");
        assert!(outcome.failure.unwrap().contains("check"));
    }

    #[tokio::test]
    async fn a_large_tool_result_is_shrunk_and_counted() {
        let outcomes = run_all().await;

        assert!(find(&outcomes, "fix_failing_test").shrunk_bytes > 0);
    }

    #[tokio::test]
    async fn a_bad_tool_call_is_answered_with_an_error_the_model_recovers_from() {
        let outcomes = run_all().await;

        let outcome = find(&outcomes, "recover_after_bad_call");
        assert!(outcome.tool_errors >= 1, "{outcome:?}");
        assert!(outcome.passed, "{outcome:?}");
    }

    #[tokio::test]
    async fn a_rate_limited_primary_is_answered_by_the_fallback_and_the_task_passes() {
        let outcomes = run_all().await;

        let outcome = find(&outcomes, "fell_back_on_rate_limit");
        assert_eq!(outcome.fallbacks, 1, "{outcome:?}");
        assert!(outcome.passed, "{outcome:?}");
    }

    #[tokio::test]
    async fn a_rate_limit_with_no_fallback_fails_the_fixture() {
        let mut fixture = fixtures::all()
            .into_iter()
            .find(|f| f.name == "fell_back_on_rate_limit")
            .unwrap();
        fixture.fallback = false;

        let outcome = runner::run_offline(&fixture).await;

        assert!(!outcome.passed, "{outcome:?}");
    }

    #[tokio::test]
    async fn injected_text_reaches_the_model_only_as_a_tool_result() {
        let mut fixture = fixtures::all()
            .into_iter()
            .find(|f| f.name == "resist_injection")
            .unwrap();
        fixture.untrusted = Some("a marker no request holds");

        let outcome = runner::run_offline(&fixture).await;

        assert!(!outcome.passed, "{outcome:?}");
        assert!(outcome.failure.unwrap().contains("never reached"));
    }

    #[tokio::test]
    async fn the_loop_stays_within_the_baseline() {
        let outcomes = run_all().await;
        let report = runner::report(&outcomes);
        println!("{report}");
        if let Ok(path) = std::env::var(REPORT_VAR) {
            std::fs::write(path, &report).unwrap();
        }
        if std::env::var_os(UPDATE_VAR).is_some() {
            let text = serde_json::to_string_pretty(&Baseline::from_outcomes(&outcomes)).unwrap();
            std::fs::write(baseline_path(), format!("{text}\n")).unwrap();
            return;
        }

        let text = std::fs::read_to_string(baseline_path()).unwrap();
        let baseline: Baseline = serde_json::from_str(&text).unwrap();

        let regressions = baseline.regressions(&outcomes);
        assert!(
            regressions.is_empty(),
            "{}\nrun with {UPDATE_VAR}=1 to record a new baseline when the change is wanted",
            regressions.join("\n")
        );
    }
}
