// SPDX-License-Identifier: Apache-2.0

//! A review fixture with known false positives: a reviewer reply with two real findings and
//! three false or doubtful ones, and the scores a scripted verifier gives each. It checks
//! that the step that applies a verifier's scores drops the false ones and keeps and ranks
//! the real ones, and that the verifier is shown each finding and the lines it points at.

use coda_review::diff::parse_diff;
use coda_review::ignore::IgnoreSet;
use coda_review::pack::{PackOptions, pack};
use coda_review::parse::parse_model_output;
use coda_review::verify::{apply, input_text, parse_verdicts};

const CHANGE: &str = include_str!("fixtures/verify/change.diff");
const REVIEWER_REPLY: &str = include_str!("fixtures/verify/reviewer_reply.json");
const VERDICTS: &str = include_str!("fixtures/verify/verdicts.json");
const BUDGET: usize = 10_000;
const DEFAULT_THRESHOLD: u8 = 4;
const KNOWN_FALSE: [&str; 3] = [
    "Lock is held across an await",
    "Cache does not derive Debug",
    "get has no doc comment",
];
const KNOWN_REAL: [&str; 2] = [
    "Poisoned lock panics the reader",
    "One mutex serializes all readers",
];

#[test]
fn the_scores_of_a_fake_verifier_drop_every_known_false_positive_and_rank_the_real_findings() {
    // Arrange
    let ignore = IgnoreSet::new(&[], false).unwrap();
    let packed = pack(
        &parse_diff(CHANGE),
        PackOptions {
            budget: BUDGET,
            ignore: &ignore,
            main_extension: None,
        },
    );
    let reviewed = parse_model_output(REVIEWER_REPLY, &packed.scope, 10).unwrap();
    assert_eq!(reviewed.findings.len(), 5);
    let asked = input_text(&reviewed.findings, &[packed.text.as_str()]);
    let verdicts = parse_verdicts(VERDICTS, &reviewed.findings).unwrap();

    // Act
    let applied = apply(reviewed.findings, Some(&verdicts), DEFAULT_THRESHOLD);

    // Assert
    let titles: Vec<&str> = applied.findings.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(titles, KNOWN_REAL);
    for false_positive in KNOWN_FALSE {
        assert!(!titles.contains(&false_positive), "{false_positive}");
    }
    assert_eq!(applied.dropped, KNOWN_FALSE.len());
    assert_eq!(applied.findings[0].score, Some(7));
    assert!(applied.findings.iter().all(|f| !f.unverified));
    for title in KNOWN_FALSE.iter().chain(&KNOWN_REAL) {
        assert!(asked.contains(title), "the verifier was not shown {title}");
    }
    assert!(asked.contains("let items = self.items.lock().unwrap();"));
}

#[test]
fn a_verifier_that_fails_drops_nothing_so_the_false_positives_stay_but_are_marked() {
    // Arrange
    let ignore = IgnoreSet::new(&[], false).unwrap();
    let packed = pack(
        &parse_diff(CHANGE),
        PackOptions {
            budget: BUDGET,
            ignore: &ignore,
            main_extension: None,
        },
    );
    let reviewed = parse_model_output(REVIEWER_REPLY, &packed.scope, 10).unwrap();

    // Act
    let applied = apply(reviewed.findings, None, DEFAULT_THRESHOLD);

    // Assert
    assert_eq!(applied.findings.len(), 5);
    assert!(applied.findings.iter().all(|f| f.unverified));
}
