// SPDX-License-Identifier: Apache-2.0

//! The cache hint: a large session that sat idle past the prompt cache's lifetime
//! resends its whole context at full price with the next message, so the UI says
//! so and offers to compact first.

use std::time::Duration;

const THOUSAND: u64 = 1_000;
const MILLION: u64 = 1_000_000;

/// When the hint applies. A zero lifetime turns it off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheHint {
    /// How long without a message the cache is assumed to last.
    pub ttl: Duration,
    /// The smallest context, in tokens, that gets the hint.
    pub min_tokens: u64,
}

impl CacheHint {
    /// Whether a session idle for `idle` with `tokens` of context gets the hint.
    #[must_use]
    pub fn applies(&self, idle: Duration, tokens: u64) -> bool {
        !self.ttl.is_zero() && idle >= self.ttl && tokens >= self.min_tokens
    }
}

/// The line shown when the hint applies. `can_compact` says whether Tab compacts.
#[must_use]
pub fn text(tokens: u64, can_compact: bool) -> String {
    let tail = if can_compact {
        ", Tab to compact first"
    } else {
        ""
    };
    format!(
        "cache likely expired: the next message resends about {} tokens{tail}",
        approx(tokens)
    )
}

/// A token count rounded down to thousands or millions, such as `62k`.
fn approx(tokens: u64) -> String {
    if tokens >= MILLION {
        format!(
            "{}.{}M",
            tokens / MILLION,
            tokens % MILLION / (MILLION / 10)
        )
    } else if tokens >= THOUSAND {
        format!("{}k", tokens / THOUSAND)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HINT: CacheHint = CacheHint {
        ttl: Duration::from_secs(300),
        min_tokens: 50_000,
    };

    #[test]
    fn the_hint_needs_a_long_idle_time_and_a_large_context() {
        // Arrange
        let long = Duration::from_secs(300);
        let short = Duration::from_secs(299);

        // Act
        let both = HINT.applies(long, 50_000);
        let too_recent = HINT.applies(short, 90_000);
        let too_small = HINT.applies(long, 49_999);

        // Assert
        assert!(both);
        assert!(!too_recent);
        assert!(!too_small);
    }

    #[test]
    fn a_zero_lifetime_turns_the_hint_off() {
        // Arrange
        let off = CacheHint {
            ttl: Duration::ZERO,
            min_tokens: 0,
        };

        // Act
        let applies = off.applies(Duration::from_hours(24), 1_000_000);

        // Assert
        assert!(!applies);
    }

    #[test]
    fn the_text_names_the_size_and_offers_tab_only_when_it_compacts() {
        // Act
        let with = text(62_400, true);
        let without = text(62_400, false);

        // Assert
        assert_eq!(
            with,
            "cache likely expired: the next message resends about 62k tokens, Tab to compact first"
        );
        assert_eq!(
            without,
            "cache likely expired: the next message resends about 62k tokens"
        );
    }

    #[test]
    fn counts_read_in_tokens_thousands_and_millions() {
        for (tokens, expected) in [
            (900, "900"),
            (1_000, "1k"),
            (999_999, "999k"),
            (1_250_000, "1.2M"),
        ] {
            // Act
            let shown = approx(tokens);

            // Assert
            assert_eq!(shown, expected, "{tokens}");
        }
    }
}
