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
}

impl CommandDef {
    /// The prompt for `args`, without the blank space the file puts around it.
    ///
    /// # Errors
    /// Returns [`ExpandError::NoPlaceholder`] when `args` is not blank and the
    /// template has no placeholder, so the arguments are never dropped silently.
    pub fn expand(&self, args: &str) -> Result<String, ExpandError> {
        let args = args.trim();
        let has_placeholder = self.body.contains(ARGUMENTS_PLACEHOLDER);
        if !has_placeholder && !args.is_empty() {
            return Err(ExpandError::NoPlaceholder);
        }
        // One pass over the template: text the arguments bring is never
        // scanned again, so a placeholder typed in them stays as typed.
        Ok(self
            .body
            .replace(ARGUMENTS_PLACEHOLDER, args)
            .trim()
            .to_owned())
    }
}
