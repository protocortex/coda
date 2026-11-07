// SPDX-License-Identifier: Apache-2.0

//! What a Claude Code session says it loaded, checked against what its profile
//! was meant to leave out.

use coda_config::config::ClaudeProfile;
use serde_json::Value;

/// The tools of the lean profile, as `--tools` names them.
pub const LEAN_TOOLS: &[&str] = &["Read", "Grep", "Glob", "Edit", "Write", "Bash"];

const LIST_SEPARATOR: &str = ", ";

/// The names a Claude Code `init` message lists. Only names are kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InitReport {
    /// Tools the model can call.
    pub tools: Vec<String>,
    /// Connected or failed MCP servers.
    pub mcp_servers: Vec<String>,
    /// Skills that were loaded.
    pub skills: Vec<String>,
}

impl InitReport {
    /// Reads the lists from an `init` message; a missing or odd list is empty.
    #[must_use]
    pub fn from_init(init: &Value) -> Self {
        Self {
            tools: names(&init["tools"]),
            mcp_servers: names(&init["mcp_servers"]),
            skills: names(&init["skills"]),
        }
    }

    /// One line for each kind of thing `profile` should have removed and did not.
    #[must_use]
    pub fn unexpected(&self, profile: ClaudeProfile) -> Vec<String> {
        if profile == ClaudeProfile::Full {
            return Vec::new();
        }
        let extra_tools: Vec<&str> = if profile == ClaudeProfile::Lean {
            self.tools
                .iter()
                .map(String::as_str)
                .filter(|tool| !LEAN_TOOLS.contains(tool))
                .collect()
        } else {
            Vec::new()
        };
        let found = [
            ("tools outside the profile's list", extra_tools),
            ("MCP servers", as_strs(&self.mcp_servers)),
            ("skills", as_strs(&self.skills)),
        ];
        found
            .into_iter()
            .filter(|(_, items)| !items.is_empty())
            .map(|(what, items)| {
                format!(
                    "the {} profile should have removed {what}, but the session has {}",
                    profile.as_str(),
                    items.join(LIST_SEPARATOR)
                )
            })
            .collect()
    }
}

fn as_strs(items: &[String]) -> Vec<&str> {
    items.iter().map(String::as_str).collect()
}

/// The names in a list whose entries are strings or objects with a `name`.
fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.as_str().or_else(|| entry["name"].as_str()))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn report(tools: &[&str], mcp: &[&str], skills: &[&str]) -> InitReport {
        let owned = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect();
        InitReport {
            tools: owned(tools),
            mcp_servers: owned(mcp),
            skills: owned(skills),
        }
    }

    #[test]
    fn names_are_read_from_strings_and_from_objects() {
        // Arrange
        let init = json!({
            "tools": ["Read", "Bash"],
            "mcp_servers": [{"name": "docs", "status": "connected"}, "plain"],
            "skills": ["review"],
        });

        // Act
        let report = InitReport::from_init(&init);

        // Assert
        assert_eq!(report.tools, ["Read", "Bash"]);
        assert_eq!(report.mcp_servers, ["docs", "plain"]);
        assert_eq!(report.skills, ["review"]);
    }

    #[test]
    fn missing_or_odd_lists_are_empty() {
        // Act
        let report = InitReport::from_init(&json!({"tools": "Read", "skills": [1, null]}));

        // Assert
        assert_eq!(report, InitReport::default());
    }

    #[test]
    fn the_full_profile_expects_nothing_removed() {
        // Arrange
        let loaded = report(&["Read", "WebFetch"], &["docs"], &["review"]);

        // Act
        let found = loaded.unexpected(ClaudeProfile::Full);

        // Assert
        assert_eq!(found, Vec::<String>::new());
    }

    #[test]
    fn the_project_profile_expects_no_mcp_servers_and_no_skills_but_any_tool() {
        // Arrange
        let loaded = report(&["Read", "WebFetch"], &["docs", "git"], &["review"]);

        // Act
        let found = loaded.unexpected(ClaudeProfile::Project);

        // Assert
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].contains("MCP servers") && found[0].contains("docs, git"));
        assert!(found[1].contains("skills") && found[1].contains("review"));
    }

    #[test]
    fn the_lean_profile_also_expects_only_its_tools() {
        // Arrange
        let loaded = report(&["Read", "WebFetch", "Task"], &[], &[]);

        // Act
        let found = loaded.unexpected(ClaudeProfile::Lean);

        // Assert
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].contains("tools") && found[0].contains("WebFetch, Task"));
    }

    #[test]
    fn a_session_that_loaded_only_what_the_profile_allows_reports_nothing() {
        // Arrange
        let loaded = report(LEAN_TOOLS, &[], &[]);

        // Act
        let found = loaded.unexpected(ClaudeProfile::Lean);

        // Assert
        assert_eq!(found, Vec::<String>::new());
    }
}
