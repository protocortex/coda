// SPDX-License-Identifier: Apache-2.0

//! Detects a model that repeats itself: the same tool call with the same
//! result several times, or two calls taking turns with no change in their
//! results. Pure, so it is tested without a model.

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};

use coda_config::loop_guard::LoopGuardConfig;

/// Longest cycle of distinct calls the guard recognizes.
const MAX_PERIOD: usize = 2;

/// What the guard makes of the call it was just shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing worth saying.
    Fine,
    /// The model is repeating itself; it should be told once.
    Warn(Repeat),
    /// It went on after the warning; the turn should end.
    Stop(Repeat),
}

/// What repeated, for the words shown to the model and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repeat {
    /// Tools of one cycle, oldest first: one name, or two when calls alternate.
    pub tools: Vec<String>,
    /// How many times the cycle has now run.
    pub times: u32,
}

impl Repeat {
    /// The sentence that tells the model what repeated.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.tools.as_slice() {
            [one] => format!(
                "you called `{one}` {} times with the same arguments and got the same result each time",
                self.times
            ),
            [first, second] if first == second => format!(
                "you made two different `{first}` calls {} times each and got the same results each time",
                self.times
            ),
            _ => format!(
                "you alternated between {} {} times with the same arguments and got the same results each time",
                self.tools
                    .iter()
                    .map(|tool| format!("`{tool}`"))
                    .collect::<Vec<_>>()
                    .join(" and "),
                self.times
            ),
        }
    }
}

/// A number that is the same for the same output text.
#[must_use]
pub fn digest(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

struct Seen {
    tool: String,
    fingerprint: u64,
}

/// Remembers the recent calls of one send.
pub struct LoopGuard {
    config: LoopGuardConfig,
    recent: VecDeque<Seen>,
}

impl LoopGuard {
    /// A guard with nothing seen yet.
    #[must_use]
    pub fn new(config: LoopGuardConfig) -> Self {
        Self {
            config,
            recent: VecDeque::new(),
        }
    }

    /// Forgets everything seen, as a new user message starts a fresh turn.
    pub fn reset(&mut self) {
        self.recent.clear();
    }

    /// Records a finished call and judges the run of calls it ends. `result` is
    /// the [`digest`] of the tool's own output, taken before it was shortened.
    pub fn record(&mut self, tool: &str, arguments: &str, result: u64, is_error: bool) -> Verdict {
        self.recent.push_back(Seen {
            tool: tool.to_owned(),
            fingerprint: fingerprint(tool, arguments, result, is_error),
        });
        let kept = MAX_PERIOD * self.config.stop_after as usize;
        while self.recent.len() > kept {
            self.recent.pop_front();
        }
        let Some((period, times)) = self.longest_cycle() else {
            return Verdict::Fine;
        };
        let repeat = || Repeat {
            tools: self
                .recent
                .iter()
                .skip(self.recent.len() - period)
                .map(|seen| seen.tool.clone())
                .collect(),
            times,
        };
        if times >= self.config.stop_after {
            Verdict::Stop(repeat())
        } else if times == self.config.warn_after {
            Verdict::Warn(repeat())
        } else {
            Verdict::Fine
        }
    }

    /// The cycle length and the times it has just run, for the cycle that ran
    /// most often and ends with the last call. A cycle that is only part way
    /// round is not judged, so a verdict is given once per round.
    fn longest_cycle(&self) -> Option<(usize, u32)> {
        let mut best: Option<(usize, u32)> = None;
        for period in 1..=MAX_PERIOD {
            let mut run = 0;
            let mut at = self.recent.len();
            while at > period
                && self.recent[at - 1].fingerprint == self.recent[at - 1 - period].fingerprint
            {
                run += 1;
                at -= 1;
            }
            if run % period != 0 {
                continue;
            }
            let times = u32::try_from((run + period) / period).unwrap_or(u32::MAX);
            if times >= 2 && best.is_none_or(|(_, most)| times > most) {
                best = Some((period, times));
            }
        }
        best
    }
}

fn fingerprint(tool: &str, arguments: &str, result: u64, is_error: bool) -> u64 {
    let mut hasher = DefaultHasher::new();
    (tool, arguments, result, is_error).hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> LoopGuard {
        LoopGuard::new(LoopGuardConfig::default())
    }

    fn call(guard: &mut LoopGuard, tool: &str, arguments: &str, result: &str) -> Verdict {
        guard.record(tool, arguments, digest(result), false)
    }

    #[test]
    fn one_call_repeated_warns_on_the_third_and_stops_on_the_fourth() {
        // Arrange
        let mut guard = guard();

        // Act
        let verdicts: Vec<Verdict> = (0..4)
            .map(|_| call(&mut guard, "read", r#"{"path":"a"}"#, "same"))
            .collect();

        // Assert
        let repeat = |times| Repeat {
            tools: vec!["read".to_owned()],
            times,
        };
        assert_eq!(
            verdicts,
            [
                Verdict::Fine,
                Verdict::Fine,
                Verdict::Warn(repeat(3)),
                Verdict::Stop(repeat(4)),
            ]
        );
    }

    #[test]
    fn a_different_argument_resets_the_count() {
        // Arrange
        let mut guard = guard();
        call(&mut guard, "read", "a", "same");
        call(&mut guard, "read", "a", "same");

        // Act
        let other = call(&mut guard, "read", "b", "same");
        let back = call(&mut guard, "read", "a", "same");

        // Assert
        assert_eq!((other, back), (Verdict::Fine, Verdict::Fine));
    }

    #[test]
    fn a_changed_result_resets_the_count() {
        // Arrange
        let mut guard = guard();

        // Act
        let verdicts: Vec<Verdict> = ["1", "2", "3", "4", "5", "6"]
            .into_iter()
            .map(|out| call(&mut guard, "poll", "{}", out))
            .collect();

        // Assert
        assert!(verdicts.iter().all(|v| *v == Verdict::Fine), "{verdicts:?}");
    }

    #[test]
    fn a_poll_that_settles_after_changing_is_counted_from_the_settling() {
        // Arrange
        let mut guard = guard();
        for out in ["1", "2"] {
            call(&mut guard, "poll", "{}", out);
        }

        // Act
        let verdicts: Vec<Verdict> = (0..3)
            .map(|_| call(&mut guard, "poll", "{}", "done"))
            .collect();

        // Assert
        assert!(matches!(verdicts[2], Verdict::Warn(_)), "{verdicts:?}");
    }

    #[test]
    fn an_error_and_a_success_with_the_same_text_are_not_the_same_result() {
        // Arrange
        let mut guard = guard();
        guard.record("bash", "ls", digest("x"), true);
        guard.record("bash", "ls", digest("x"), false);

        // Act
        let third = guard.record("bash", "ls", digest("x"), true);

        // Assert
        assert_eq!(third, Verdict::Fine);
    }

    #[test]
    fn two_calls_taking_turns_warn_on_the_third_cycle_and_stop_on_the_fourth() {
        // Arrange
        let mut guard = guard();
        let mut verdicts = Vec::new();

        // Act
        for _ in 0..4 {
            verdicts.push(call(&mut guard, "read", "a", "r1"));
            verdicts.push(call(&mut guard, "grep", "b", "r2"));
        }

        // Assert
        let repeat = |times| Repeat {
            tools: vec!["read".to_owned(), "grep".to_owned()],
            times,
        };
        assert_eq!(verdicts[5], Verdict::Warn(repeat(3)));
        assert_eq!(verdicts[7], Verdict::Stop(repeat(4)));
        assert!(verdicts[..5].iter().all(|v| *v == Verdict::Fine));
    }

    #[test]
    fn taking_turns_with_a_changing_result_is_not_a_loop() {
        // Arrange
        let mut guard = guard();
        let mut verdicts = Vec::new();

        // Act
        for round in 0..6 {
            verdicts.push(call(&mut guard, "edit", "a", &format!("v{round}")));
            verdicts.push(call(&mut guard, "test", "b", "fail"));
        }

        // Assert
        assert!(verdicts.iter().all(|v| *v == Verdict::Fine), "{verdicts:?}");
    }

    #[test]
    fn a_repeat_after_a_break_warns_again() {
        // Arrange
        let mut guard = guard();
        for _ in 0..3 {
            call(&mut guard, "read", "a", "same");
        }
        call(&mut guard, "read", "b", "other");

        // Act
        let verdicts: Vec<Verdict> = (0..3)
            .map(|_| call(&mut guard, "read", "a", "same"))
            .collect();

        // Assert
        assert!(matches!(verdicts[2], Verdict::Warn(_)), "{verdicts:?}");
    }

    #[test]
    fn reset_forgets_the_calls_seen() {
        // Arrange
        let mut guard = guard();
        call(&mut guard, "read", "a", "same");
        call(&mut guard, "read", "a", "same");
        guard.reset();

        // Act
        let after = call(&mut guard, "read", "a", "same");

        // Assert
        assert_eq!(after, Verdict::Fine);
    }

    #[test]
    fn thresholds_come_from_the_config() {
        // Arrange
        let mut guard = LoopGuard::new(LoopGuardConfig {
            warn_after: 2,
            stop_after: 6,
        });

        // Act
        let verdicts: Vec<Verdict> = (0..6)
            .map(|_| call(&mut guard, "read", "a", "same"))
            .collect();

        // Assert
        assert!(matches!(verdicts[1], Verdict::Warn(_)));
        assert_eq!(verdicts[2], Verdict::Fine);
        assert!(matches!(verdicts[5], Verdict::Stop(_)));
    }

    #[test]
    fn memory_stays_bounded_over_a_long_run() {
        // Arrange
        let mut guard = guard();

        // Act
        for n in 0..1000 {
            call(&mut guard, "read", &n.to_string(), "x");
        }

        // Assert
        assert!(guard.recent.len() <= MAX_PERIOD * 4);
    }

    #[test]
    fn the_warning_names_what_repeated() {
        // Arrange
        let one = Repeat {
            tools: vec!["read".to_owned()],
            times: 3,
        };
        let two = Repeat {
            tools: vec!["read".to_owned(), "grep".to_owned()],
            times: 3,
        };

        // Assert
        assert!(one.describe().contains("`read` 3 times"));
        assert!(two.describe().contains("`read` and `grep` 3 times"));
    }
}
