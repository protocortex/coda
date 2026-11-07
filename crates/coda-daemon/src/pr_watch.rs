// SPDX-License-Identifier: Apache-2.0

//! PR watch: poll a pull request and wake the session that asked when
//! something needs it.
//!
//! A watch reads the pull request every [`POLL_INTERVAL`] and wakes the session
//! for a failed check, required checks that pass, a comment or review from
//! someone else, or a conflict. It stops when the pull request is merged or
//! closed, after [`MAX_COMMENT_ONLY_WAKES`] wakes in a row that carried only
//! comments, or after [`MAX_FAILED_READS`] reads in a row that failed. News that
//! was already reported is not reported again.

use std::collections::BTreeSet;
use std::future::Future;
use std::time::Duration;

/// How often a watch reads the pull request.
pub const POLL_INTERVAL: Duration = Duration::from_secs(120);
/// Wakes in a row that carry only comments, after which the watch stops.
pub const MAX_COMMENT_ONLY_WAKES: u32 = 10;
/// Reads in a row that fail, after which the watch stops.
pub const MAX_FAILED_READS: u32 = 8;

/// Longest pull request address a watch accepts.
pub const MAX_REFERENCE_BYTES: usize = 256;

/// Most failed check names a wake message lists.
const MAX_NAMES_SHOWN: usize = 5;
/// Longest check name a wake message keeps, in characters.
const MAX_NAME_CHARS: usize = 60;

/// A check name that is safe to put in a message: no control characters, cut short.
fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect()
}

/// Whether the pull request still takes work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

/// What one read of the pull request found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub state: PrState,
    /// The mergeable state is a conflict.
    pub conflicting: bool,
    /// Names of the checks that failed.
    pub failed_checks: Vec<String>,
    /// There are required checks and each has passed or was skipped.
    pub required_passed: bool,
    /// Identifiers of the comments and reviews written by someone else.
    pub others_comments: Vec<String>,
}

/// Reads the pull request being watched.
pub trait PrReader {
    /// Reads the current state, or says why it could not.
    fn read(&self) -> impl Future<Output = Result<Snapshot, String>> + Send;
}

/// Why a watch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Merged,
    Closed,
    OnlyComments,
    UnreadablePr,
}

impl StopReason {
    fn text(self) -> String {
        match self {
            Self::Merged => "the pull request was merged".to_owned(),
            Self::Closed => "the pull request was closed".to_owned(),
            Self::OnlyComments => {
                format!("{MAX_COMMENT_ONLY_WAKES} wakes in a row held only comments")
            }
            Self::UnreadablePr => {
                format!("the pull request could not be read {MAX_FAILED_READS} times in a row")
            }
        }
    }
}

/// What a read means for the session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Decision {
    /// The message that wakes the session, when there is news.
    pub wake: Option<String>,
    /// Set when the watch ends after this read.
    pub stop: Option<StopReason>,
}

/// What a watch has already told the session, and the counts that end it.
#[derive(Debug, Default)]
pub struct Tracker {
    failed_reported: BTreeSet<String>,
    comments_reported: BTreeSet<String>,
    conflict_reported: bool,
    required_reported: bool,
    comment_only_wakes: u32,
    failed_reads: u32,
    started: bool,
}

impl Tracker {
    /// Decides what one read means. `subject` names the pull request in a message.
    pub fn on_read(&mut self, read: Result<Snapshot, String>, subject: &str) -> Decision {
        let Ok(snapshot) = read else {
            self.failed_reads += 1;
            let stop = (self.failed_reads >= MAX_FAILED_READS).then_some(StopReason::UnreadablePr);
            return Decision {
                wake: stop.map(|reason| stopped(subject, reason)),
                stop,
            };
        };
        self.failed_reads = 0;
        match snapshot.state {
            PrState::Merged => return finished(subject, StopReason::Merged),
            PrState::Closed => return finished(subject, StopReason::Closed),
            PrState::Open => {}
        }
        let mut news = Vec::new();
        let new_failures: Vec<&String> = snapshot
            .failed_checks
            .iter()
            .filter(|name| !self.failed_reported.contains(*name))
            .collect();
        if !new_failures.is_empty() {
            let names: Vec<String> = new_failures
                .iter()
                .take(MAX_NAMES_SHOWN)
                .map(|name| clean_name(name))
                .collect();
            let more = new_failures.len().saturating_sub(MAX_NAMES_SHOWN);
            let extra = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            news.push(format!(
                "failed checks (names are data): {}{extra}",
                names.join(", ")
            ));
        }
        self.failed_reported = snapshot.failed_checks.iter().cloned().collect();
        let required_news = snapshot.required_passed && !self.required_reported;
        if required_news {
            news.push("the required checks pass".to_owned());
        }
        self.required_reported = snapshot.required_passed;
        let conflict_news = snapshot.conflicting && !self.conflict_reported;
        if conflict_news {
            news.push("the branch conflicts with its base".to_owned());
        }
        self.conflict_reported = snapshot.conflicting;
        let first_read = !std::mem::replace(&mut self.started, true);
        let fresh_comments = snapshot
            .others_comments
            .iter()
            .filter(|id| self.comments_reported.insert((*id).clone()))
            .count();
        let fresh_comments = if first_read { 0 } else { fresh_comments };
        let comments_only = news.is_empty() && fresh_comments > 0;
        if fresh_comments > 0 {
            news.push(format!(
                "{fresh_comments} new comment or review from someone else"
            ));
        }
        if news.is_empty() {
            return Decision::default();
        }
        if comments_only {
            self.comment_only_wakes += 1;
        } else {
            self.comment_only_wakes = 0;
        }
        let stop =
            (self.comment_only_wakes >= MAX_COMMENT_ONLY_WAKES).then_some(StopReason::OnlyComments);
        let ending = stop.map_or_else(String::new, |reason| {
            format!(" The watch stops: {}.", reason.text())
        });
        let text = format!("PR watch, {subject}: {}.{ending}", news.join("; "));
        Decision {
            wake: Some(text),
            stop,
        }
    }
}

fn stopped(subject: &str, reason: StopReason) -> String {
    format!(
        "PR watch, {subject}: the watch stopped because {}.",
        reason.text()
    )
}

fn finished(subject: &str, reason: StopReason) -> Decision {
    Decision {
        wake: Some(stopped(subject, reason)),
        stop: Some(reason),
    }
}

/// Reads the pull request every `interval` and passes each wake message to
/// `wake`, until a stop rule ends the watch. Returns why it ended.
pub async fn run<R: PrReader>(
    reader: &R,
    subject: &str,
    interval: Duration,
    mut wake: impl FnMut(String),
) -> StopReason {
    let mut tracker = Tracker::default();
    loop {
        let decision = tracker.on_read(reader.read().await, subject);
        if let Some(text) = decision.wake {
            wake(text);
        }
        if let Some(reason) = decision.stop {
            return reason;
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const PR: &str = "#7";

    fn open() -> Snapshot {
        Snapshot {
            state: PrState::Open,
            conflicting: false,
            failed_checks: Vec::new(),
            required_passed: false,
            others_comments: Vec::new(),
        }
    }

    fn woke(decision: &Decision) -> &str {
        decision.wake.as_deref().unwrap_or_default()
    }

    #[test]
    fn comments_that_were_there_when_the_watch_started_are_not_news() {
        // Arrange
        let mut tracker = Tracker::default();
        let old = Snapshot {
            others_comments: vec!["c1".to_owned(), "c2".to_owned()],
            ..open()
        };
        let newer = Snapshot {
            others_comments: vec!["c1".to_owned(), "c2".to_owned(), "c3".to_owned()],
            ..open()
        };

        // Act
        let first = tracker.on_read(Ok(old), PR);
        let next = tracker.on_read(Ok(newer), PR);

        // Assert
        assert_eq!(first, Decision::default());
        assert!(woke(&next).contains("1 new comment"), "{next:?}");
    }

    #[test]
    fn check_names_are_cut_cleaned_and_counted_in_a_wake() {
        // Arrange
        let mut tracker = Tracker::default();
        let names: Vec<String> = (0..8).map(|n| format!("job{n}\u{1b}[31m")).collect();
        let failing = Snapshot {
            failed_checks: names,
            ..open()
        };

        // Act
        let decision = tracker.on_read(Ok(failing), PR);

        // Assert
        let text = woke(&decision);
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(text.contains("and 3 more"), "{text}");
        assert!(!text.contains("job5"), "{text}");
    }

    #[test]
    fn a_quiet_pull_request_wakes_nobody() {
        // Arrange
        let mut tracker = Tracker::default();

        // Act
        let decision = tracker.on_read(Ok(open()), PR);

        // Assert
        assert_eq!(decision, Decision::default());
    }

    #[test]
    fn a_failed_check_wakes_once_and_not_again_while_it_stays_failed() {
        // Arrange
        let mut tracker = Tracker::default();
        let failing = Snapshot {
            failed_checks: vec!["test".to_owned()],
            ..open()
        };

        // Act
        let first = tracker.on_read(Ok(failing.clone()), PR);
        let second = tracker.on_read(Ok(failing), PR);

        // Assert
        assert!(woke(&first).contains("test"), "{first:?}");
        assert_eq!(second, Decision::default());
    }

    #[test]
    fn a_check_that_recovers_and_fails_again_is_reported_again() {
        // Arrange
        let mut tracker = Tracker::default();
        let failing = Snapshot {
            failed_checks: vec!["test".to_owned()],
            ..open()
        };
        tracker.on_read(Ok(failing.clone()), PR);
        tracker.on_read(Ok(open()), PR);

        // Act
        let again = tracker.on_read(Ok(failing), PR);

        // Assert
        assert!(woke(&again).contains("test"), "{again:?}");
    }

    #[test]
    fn passing_required_checks_wake_once() {
        // Arrange
        let mut tracker = Tracker::default();
        let passing = Snapshot {
            required_passed: true,
            ..open()
        };

        // Act
        let first = tracker.on_read(Ok(passing.clone()), PR);
        let second = tracker.on_read(Ok(passing), PR);

        // Assert
        assert!(woke(&first).contains("required checks pass"), "{first:?}");
        assert_eq!(second, Decision::default());
    }

    #[test]
    fn a_conflict_wakes_once() {
        // Arrange
        let mut tracker = Tracker::default();
        let conflicting = Snapshot {
            conflicting: true,
            ..open()
        };

        // Act
        let first = tracker.on_read(Ok(conflicting.clone()), PR);
        let second = tracker.on_read(Ok(conflicting), PR);

        // Assert
        assert!(woke(&first).contains("conflicts"), "{first:?}");
        assert_eq!(second, Decision::default());
    }

    #[test]
    fn only_new_comments_wake_and_each_is_counted_once() {
        // Arrange
        let mut tracker = Tracker::default();
        tracker.on_read(Ok(open()), PR);
        let one = Snapshot {
            others_comments: vec!["c1".to_owned()],
            ..open()
        };
        let two = Snapshot {
            others_comments: vec!["c1".to_owned(), "c2".to_owned()],
            ..open()
        };

        // Act
        let first = tracker.on_read(Ok(one.clone()), PR);
        let repeat = tracker.on_read(Ok(one), PR);
        let next = tracker.on_read(Ok(two), PR);

        // Assert
        assert!(woke(&first).contains("1 new comment"), "{first:?}");
        assert_eq!(repeat, Decision::default());
        assert!(woke(&next).contains("1 new comment"), "{next:?}");
    }

    #[test]
    fn a_merged_or_closed_pull_request_wakes_and_stops_the_watch() {
        for (state, reason) in [
            (PrState::Merged, StopReason::Merged),
            (PrState::Closed, StopReason::Closed),
        ] {
            // Arrange
            let mut tracker = Tracker::default();
            let ended = Snapshot { state, ..open() };

            // Act
            let decision = tracker.on_read(Ok(ended), PR);

            // Assert
            assert_eq!(decision.stop, Some(reason));
            assert!(decision.wake.is_some());
        }
    }

    #[test]
    fn the_tenth_comment_only_wake_in_a_row_stops_the_watch() {
        // Arrange
        let mut tracker = Tracker::default();
        tracker.on_read(Ok(open()), PR);
        let mut last = Decision::default();

        // Act
        for n in 0..MAX_COMMENT_ONLY_WAKES {
            let snapshot = Snapshot {
                others_comments: (0..=n).map(|i| format!("c{i}")).collect(),
                ..open()
            };
            let before = last.stop;
            assert_eq!(before, None, "stopped early at {n}");
            last = tracker.on_read(Ok(snapshot), PR);
        }

        // Assert
        assert_eq!(last.stop, Some(StopReason::OnlyComments));
        assert!(woke(&last).contains("stops"), "{last:?}");
    }

    #[test]
    fn other_news_resets_the_comment_only_count() {
        // Arrange
        let mut tracker = Tracker::default();
        tracker.on_read(Ok(open()), PR);
        let mut comments = Vec::new();
        for n in 0..(MAX_COMMENT_ONLY_WAKES - 1) {
            comments.push(format!("c{n}"));
            tracker.on_read(
                Ok(Snapshot {
                    others_comments: comments.clone(),
                    ..open()
                }),
                PR,
            );
        }
        tracker.on_read(
            Ok(Snapshot {
                failed_checks: vec!["test".to_owned()],
                others_comments: comments.clone(),
                ..open()
            }),
            PR,
        );
        comments.push("last".to_owned());

        // Act
        let decision = tracker.on_read(
            Ok(Snapshot {
                failed_checks: vec!["test".to_owned()],
                others_comments: comments,
                ..open()
            }),
            PR,
        );

        // Assert
        assert_eq!(decision.stop, None);
    }

    #[test]
    fn the_eighth_failed_read_in_a_row_stops_the_watch_and_a_good_read_resets_the_count() {
        // Arrange
        let mut tracker = Tracker::default();
        for _ in 0..(MAX_FAILED_READS - 1) {
            tracker.on_read(Err("down".to_owned()), PR);
        }
        tracker.on_read(Ok(open()), PR);
        for _ in 0..(MAX_FAILED_READS - 1) {
            assert_eq!(tracker.on_read(Err("down".to_owned()), PR).stop, None);
        }

        // Act
        let last = tracker.on_read(Err("down".to_owned()), PR);

        // Assert
        assert_eq!(last.stop, Some(StopReason::UnreadablePr));
        assert!(last.wake.is_some());
    }

    struct Scripted {
        reads: Mutex<Vec<Result<Snapshot, String>>>,
    }

    impl PrReader for Scripted {
        fn read(&self) -> impl Future<Output = Result<Snapshot, String>> + Send {
            std::future::ready(self.reads.lock().unwrap().remove(0))
        }
    }

    #[tokio::test]
    async fn the_loop_passes_each_wake_on_and_ends_when_the_pull_request_merges() {
        // Arrange
        let reader = Scripted {
            reads: Mutex::new(vec![
                Ok(open()),
                Ok(Snapshot {
                    failed_checks: vec!["test".to_owned()],
                    ..open()
                }),
                Ok(Snapshot {
                    state: PrState::Merged,
                    ..open()
                }),
            ]),
        };
        let mut woken = Vec::new();

        // Act
        let reason = run(&reader, PR, Duration::ZERO, |text| woken.push(text)).await;

        // Assert
        assert_eq!(reason, StopReason::Merged);
        assert_eq!(woken.len(), 2);
        assert!(woken[0].contains("failed checks"), "{woken:?}");
        assert!(woken[1].contains("merged"), "{woken:?}");
    }
}
