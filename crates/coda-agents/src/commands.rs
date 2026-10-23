// SPDX-License-Identifier: AGPL-3.0-only

//! Which command file a name runs, and why a name may not run.
//!
//! Both front ends, the terminal UI and `coda run`, resolve a typed name here,
//! so the trust rule and its refusal text exist once.

use crate::catalog::{Catalog, Entry, Kind, PendingReason};
use crate::definition::CommandDef;

/// Why a command name cannot run.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandLookupError {
    /// No command file has this name.
    #[error("no command named {name}")]
    Unknown { name: String },
    /// Only a project file defines it, and that file is held back.
    #[error(
        "{name} is a project command that is not in use: {}; review the files, then run `coda agents trust`",
        .reason.as_str()
    )]
    Pending { name: String, reason: PendingReason },
}

impl Catalog {
    /// The command called `name`, if it may run.
    ///
    /// # Errors
    /// Returns [`CommandLookupError::Pending`] when only a project file that
    /// waits for trust defines it, and [`CommandLookupError::Unknown`] otherwise.
    pub fn runnable_command(&self, name: &str) -> Result<&Entry<CommandDef>, CommandLookupError> {
        if let Some(entry) = self.command(name) {
            return Ok(entry);
        }
        let held_back = self
            .pending()
            .iter()
            .find(|p| p.kind == Kind::Command && p.name() == Some(name));
        Err(match held_back {
            Some(pending) => CommandLookupError::Pending {
                name: name.to_owned(),
                reason: pending.reason,
            },
            None => CommandLookupError::Unknown {
                name: name.to_owned(),
            },
        })
    }
}
