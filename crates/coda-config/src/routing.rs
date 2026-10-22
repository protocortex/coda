// SPDX-License-Identifier: AGPL-3.0-only

//! Agent tiers.

use std::fmt;
use std::str::FromStr;

/// How much capability and cost an agent asks for. Agent files name a tier,
/// never a model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Cheap and quick work such as search.
    Fast,
    /// Everyday implementation and review.
    Standard,
    /// Hard reasoning.
    Deep,
}

impl Tier {
    /// Every tier, cheapest first.
    pub const ALL: [Self; 3] = [Self::Fast, Self::Standard, Self::Deep];

    /// The name used in agent files and in the `[routing]` table.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Standard => "standard",
            Self::Deep => "deep",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The text is not the name of a tier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown tier `{0}`")]
pub struct UnknownTier(pub String);

impl FromStr for Tier {
    type Err = UnknownTier;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|tier| tier.as_str() == s)
            .ok_or_else(|| UnknownTier(s.to_owned()))
    }
}
