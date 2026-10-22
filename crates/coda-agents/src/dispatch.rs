// SPDX-License-Identifier: AGPL-3.0-only

//! Which named agent a `task` call runs, and with which tools.
//!
//! Only definitions the catalog already holds can run: the user's, and the
//! project's once the repository is trusted. A project file that is still
//! pending is named in the refusal so the model can tell the user why.

use crate::catalog::{Catalog, Entry, Kind, PendingReason};
use crate::definition::AgentDef;

/// Why a `task` call cannot run its agent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DispatchError {
    /// No usable agent has this name.
    #[error("no agent named `{name}`; available: {}", .available.join(", "))]
    Unknown {
        name: String,
        /// The names that can run, sorted.
        available: Vec<String>,
    },
    /// A project file defines the agent but the repository is not trusted.
    #[error(
        "the project agent `{name}` is not in use: {}; the user must review it and run `coda agents trust`",
        .reason.as_str()
    )]
    NotTrusted { name: String, reason: PendingReason },
}

impl Catalog {
    /// The agent called `name`, if it may run.
    ///
    /// # Errors
    /// Returns [`DispatchError::NotTrusted`] when only an untrusted project
    /// file defines it, and [`DispatchError::Unknown`] otherwise.
    pub fn dispatchable(&self, name: &str) -> Result<&Entry<AgentDef>, DispatchError> {
        if let Some(entry) = self.agent(name) {
            return Ok(entry);
        }
        let held_back = self
            .pending()
            .iter()
            .find(|p| p.kind == Kind::Agent && p.path.file_stem().is_some_and(|stem| stem == name));
        match held_back {
            Some(pending) => Err(DispatchError::NotTrusted {
                name: name.to_owned(),
                reason: pending.reason,
            }),
            None => Err(DispatchError::Unknown {
                name: name.to_owned(),
                available: self.agents().map(|e| e.def.name.clone()).collect(),
            }),
        }
    }
}

/// The tools a child may call: those `def` allows that the parent has too, in
/// the agent's order. A child never has more than its parent.
#[must_use]
pub fn child_tools(def: &AgentDef, parent: &[&str]) -> Vec<String> {
    def.tools
        .iter()
        .filter(|tool| parent.contains(&tool.as_str()))
        .cloned()
        .collect()
}
