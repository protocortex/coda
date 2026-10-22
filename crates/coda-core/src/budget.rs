// SPDX-License-Identifier: AGPL-3.0-only

//! The per-session token budget.

use coda_providers::model::Usage;

/// Why a model call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the session token budget of {limit} is used up ({used} used)")]
pub struct BudgetExhausted {
    /// The session limit.
    pub limit: u64,
    /// Tokens used so far.
    pub used: u64,
}

/// Tokens a session may spend. The call that reaches the limit completes;
/// the next one is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBudget {
    limit: u64,
    used: u64,
}

impl TokenBudget {
    /// A budget of `limit` tokens with nothing used.
    #[must_use]
    pub fn new(limit: u64) -> Self {
        Self { limit, used: 0 }
    }

    /// Tokens used so far.
    #[must_use]
    pub fn used(&self) -> u64 {
        self.used
    }

    /// This budget with `used` tokens already spent, for a loop that replaces
    /// one whose calls already counted.
    #[must_use]
    pub fn with_used(self, used: u64) -> Self {
        Self { used, ..self }
    }

    /// Checks that another model call may start.
    ///
    /// # Errors
    ///
    /// Returns [`BudgetExhausted`] when the limit is already reached, which
    /// includes a zero budget.
    pub fn admit(&self) -> Result<(), BudgetExhausted> {
        if self.is_exhausted() {
            return Err(BudgetExhausted {
                limit: self.limit,
                used: self.used,
            });
        }
        Ok(())
    }

    /// Counts the tokens a finished call used.
    pub fn record(&mut self, usage: Usage) {
        self.used = self
            .used
            .saturating_add(usage.input_tokens)
            .saturating_add(usage.output_tokens);
    }

    /// Tokens left before the limit is reached.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }

    /// Whether the limit is reached, so a tool loop should stop after its
    /// current tool call.
    #[must_use]
    pub fn is_exhausted(&self) -> bool {
        self.used >= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
        }
    }

    #[test]
    fn zero_budget_refuses_before_any_call() {
        // Arrange
        let budget = TokenBudget::new(0);

        // Act
        let result = budget.admit();

        // Assert
        assert_eq!(result, Err(BudgetExhausted { limit: 0, used: 0 }));
    }

    #[test]
    fn call_below_the_limit_is_admitted() {
        // Arrange
        let mut budget = TokenBudget::new(100);

        // Act
        budget.record(usage(30, 20));

        // Assert
        assert_eq!(budget.admit(), Ok(()));
        assert!(!budget.is_exhausted());
    }

    #[test]
    fn call_that_reaches_the_limit_completes_and_the_next_is_refused() {
        // Arrange
        let mut budget = TokenBudget::new(100);
        assert_eq!(budget.admit(), Ok(()));

        // Act
        budget.record(usage(60, 40));

        // Assert
        assert!(budget.is_exhausted());
        assert_eq!(
            budget.admit(),
            Err(BudgetExhausted {
                limit: 100,
                used: 100
            })
        );
    }

    #[test]
    fn call_that_overshoots_the_limit_is_counted_in_full() {
        // Arrange
        let mut budget = TokenBudget::new(100);

        // Act
        budget.record(usage(90, 60));

        // Assert
        assert_eq!(
            budget.admit(),
            Err(BudgetExhausted {
                limit: 100,
                used: 150
            })
        );
    }

    #[test]
    fn the_remaining_tokens_shrink_with_use_and_stop_at_zero() {
        // Arrange
        let mut budget = TokenBudget::new(100);
        let fresh = budget.remaining();

        // Act
        budget.record(usage(30, 20));
        let part_used = budget.remaining();
        budget.record(usage(90, 60));

        // Assert
        assert_eq!((fresh, part_used, budget.remaining()), (100, 50, 0));
    }

    #[test]
    fn a_budget_resumed_with_spent_tokens_refuses_at_the_original_limit() {
        // Arrange
        let mut budget = TokenBudget::new(100).with_used(70);

        // Act
        budget.record(usage(20, 10));

        // Assert
        assert_eq!(budget.used(), 100);
        assert!(budget.is_exhausted());
    }

    #[test]
    fn usage_beyond_u64_saturates_instead_of_wrapping() {
        // Arrange
        let mut budget = TokenBudget::new(100);

        // Act
        budget.record(usage(u64::MAX, u64::MAX));
        budget.record(usage(1, 1));

        // Assert
        assert!(budget.is_exhausted());
    }
}
