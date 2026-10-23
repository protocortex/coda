// SPDX-License-Identifier: AGPL-3.0-only

//! The `skill` tool: the name the model, the permission policy and the loop
//! share.
//!
//! The tool itself lives in `coda-agents`, because it reads the definition
//! catalog. It is left out of [`TOOL_NAMES`](crate::TOOL_NAMES): that list is
//! what an agent definition may allow, and a sub-agent gets no skills.

/// The name the model uses to call the tool. It is also the tool name stored
/// with permission grants and decisions, so it does not change.
pub const NAME: &str = "skill";

#[cfg(test)]
mod tests {
    use super::NAME;
    use crate::TOOL_NAMES;

    #[test]
    fn the_name_is_the_one_stored_in_grants_and_no_agent_file_may_name_it() {
        // Assert
        assert_eq!(NAME, "skill");
        assert!(
            !TOOL_NAMES.contains(&NAME),
            "an agent may not be given skill"
        );
    }
}
