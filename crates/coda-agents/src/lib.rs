// SPDX-License-Identifier: AGPL-3.0-only

//! Agent, skill and command definitions: formats, loader and lint.
//!
//! See `docs/agents.md` for the file formats and the trust rules.

mod catalog;
mod definition;
mod digest;
mod frontmatter;

pub use catalog::{
    Catalog, Entry, Kind, LoadProblem, PROJECT_DIR, Pending, PendingReason, Source, revoke_project,
    trust_project,
};
pub use coda_config::routing::Tier;
pub use definition::{
    AgentDef, CommandDef, Effort, Issue, SkillDef, parse_agent, parse_command, parse_skill,
};
