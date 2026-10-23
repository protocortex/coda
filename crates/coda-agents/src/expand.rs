// SPDX-License-Identifier: AGPL-3.0-only

//! Turns a command file's prompt template into the prompt a run sends.

use crate::definition::CommandDef;

/// Where the command's arguments go in its prompt template.
pub const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";

/// Why a command's arguments could not be placed in its prompt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandError {
    /// Arguments were given but the template has nowhere to put them.
    #[error("the command takes no arguments: its prompt has no {ARGUMENTS_PLACEHOLDER}")]
    NoPlaceholder,
    /// The prompt is blank once the arguments are in place.
    #[error("the command's prompt is blank; give it arguments, or put text in the command file")]
    Blank,
}

impl CommandDef {
    /// Whether the prompt template has a place for arguments.
    #[must_use]
    pub fn takes_arguments(&self) -> bool {
        self.body.contains(ARGUMENTS_PLACEHOLDER)
    }

    /// The prompt for `args`, without the blank space the file puts around it.
    ///
    /// # Errors
    /// Returns [`ExpandError::NoPlaceholder`] when `args` is not blank and the
    /// template has no placeholder, so the arguments are never dropped silently,
    /// and [`ExpandError::Blank`] when nothing is left of the prompt, so an
    /// empty message is never sent.
    pub fn expand(&self, args: &str) -> Result<String, ExpandError> {
        let args = args.trim();
        if !self.takes_arguments() && !args.is_empty() {
            return Err(ExpandError::NoPlaceholder);
        }
        // One pass over the template: text the arguments bring is never
        // scanned again, so a placeholder typed in them stays as typed.
        let prompt = self.body.replace(ARGUMENTS_PLACEHOLDER, args);
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(ExpandError::Blank);
        }
        Ok(prompt.to_owned())
    }
}
