// SPDX-License-Identifier: AGPL-3.0-only

//! Agent, skill and command definitions: formats, loader and lint.
//!
//! See `docs/agents.md` for the file formats and the trust rules.

mod builtin;
mod catalog;
mod commands;
mod definition;
mod digest;
mod dispatch;
mod expand;
mod frontmatter;
mod result;
mod skills;

pub use catalog::{
    Catalog, Entry, Kind, LoadProblem, PROJECT_DIR, Pending, PendingReason, Source, StoredTrust,
    revoke_project, trust_project,
};
pub use coda_config::routing::Tier;
pub use commands::CommandLookupError;
pub use definition::{
    AgentDef, CommandDef, Effort, Issue, SkillDef, parse_agent, parse_command, parse_skill,
};
pub use dispatch::{DispatchError, child_tools};
pub use expand::{ARGUMENTS_PLACEHOLDER, ExpandError};
pub use result::{
    Finding, MAX_FINDING_MESSAGE_BYTES, MAX_REPORT_BYTES, MalformedReport, NO_REPLY_SUMMARY,
    Report, Severity, Status, TaskResult, parse_report,
};
pub use skills::{MAX_INDEX_BYTES, MAX_INDEX_DESCRIPTION_CHARS, MAX_SKILL_BODY_BYTES, SkillTool};
