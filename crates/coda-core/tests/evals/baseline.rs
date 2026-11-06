// SPDX-License-Identifier: Apache-2.0

//! The recorded cost of each fixture and the check that a run stays within it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::runner::Outcome;

/// Most a fixture's request tokens may exceed its baseline, in percent.
pub const DEFAULT_MARGIN_PERCENT: u64 = 10;

/// What one fixture cost when the baseline was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    pub turns: u32,
    pub tokens: u64,
}

/// The baseline file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    pub margin_percent: u64,
    pub fixtures: BTreeMap<String, Cost>,
}

impl Baseline {
    pub fn from_outcomes(outcomes: &[Outcome]) -> Self {
        Self {
            margin_percent: DEFAULT_MARGIN_PERCENT,
            fixtures: outcomes
                .iter()
                .map(|outcome| {
                    let cost = Cost {
                        turns: outcome.turns,
                        tokens: outcome.tokens,
                    };
                    (outcome.name.to_owned(), cost)
                })
                .collect(),
        }
    }

    /// One line per way `outcomes` fall short of this baseline; empty when none do.
    pub fn regressions(&self, outcomes: &[Outcome]) -> Vec<String> {
        let mut found = Vec::new();
        for outcome in outcomes {
            if !outcome.passed {
                let why = outcome.failure.as_deref().unwrap_or("no reason recorded");
                found.push(format!("{}: did not pass ({why})", outcome.name));
                continue;
            }
            let Some(cost) = self.fixtures.get(outcome.name) else {
                found.push(format!("{}: no baseline recorded", outcome.name));
                continue;
            };
            if outcome.turns > cost.turns {
                found.push(format!(
                    "{}: turns rose from {} to {}",
                    outcome.name, cost.turns, outcome.turns
                ));
            }
            let allowed = cost.tokens + cost.tokens * self.margin_percent / 100;
            if outcome.tokens > allowed {
                found.push(format!(
                    "{}: tokens rose from {} to {} (allowed {allowed})",
                    outcome.name, cost.tokens, outcome.tokens
                ));
            }
        }
        found
    }
}
