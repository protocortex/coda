// SPDX-License-Identifier: AGPL-3.0-only

//! The `skill` tool: skills are offered by name and description, and a body
//! enters the conversation only when the model asks for it.
//!
//! The index lives in the tool's description, so the model sees what it can
//! load without any body in its context, and the one place that returns a body
//! is the tool call, where the size cap applies. Only skills the catalog
//! admits are offered: the user's own, and those of a project the user
//! trusted. Their bodies are returned as instructions. A project that is not
//! trusted adds nothing, because the catalog holds none of its files.

use std::collections::BTreeMap;

use coda_tools::{Tool, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::Value;

use crate::catalog::{Catalog, Source};

/// The name the model calls the tool by.
pub const SKILL_TOOL_NAME: &str = "skill";
/// The most bytes of a skill body the tool returns; the rest is cut.
pub const MAX_SKILL_BODY_BYTES: usize = 32 * 1024;
/// The most bytes of skill entries the index lists; skills past it are left out.
pub const MAX_INDEX_BYTES: usize = 4 * 1024;
/// The most characters of one description the index shows.
pub const MAX_INDEX_DESCRIPTION_CHARS: usize = 200;

const INTRO: &str = "Load the instructions of one skill by name. Call it when the task matches a skill below, then follow what it returns. Skills:";
const ELLIPSIS: &str = "...";

#[derive(Debug, PartialEq, Eq)]
struct Loaded {
    body: String,
    source: Source,
}

/// Loads skill bodies for the model. Two tools are equal when they offer the
/// same skills with the same bodies, so a running session can tell that the
/// files or the trust decision changed.
#[derive(Debug, PartialEq, Eq)]
pub struct SkillTool {
    /// The skills the index lists, by name. A skill left out of the index
    /// cannot be loaded either, so the index is all the model can reach.
    skills: BTreeMap<String, Loaded>,
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    name: String,
}

impl SkillTool {
    /// The tool for the skills of `catalog`, or `None` when it holds none.
    #[must_use]
    pub fn from_catalog(catalog: &Catalog) -> Option<Self> {
        let mut skills = BTreeMap::new();
        let mut index = String::new();
        let mut left_out = 0_usize;
        for entry in catalog.skills() {
            let line = index_line(&entry.def.name, &entry.def.description);
            // Stop at the first entry that does not fit, so what is listed is
            // always a prefix of the alphabetical order.
            if left_out > 0 || index.len() + line.len() > MAX_INDEX_BYTES {
                left_out += 1;
                continue;
            }
            index.push_str(&line);
            skills.insert(
                entry.def.name.clone(),
                Loaded {
                    body: entry.def.body.clone(),
                    source: entry.source,
                },
            );
        }
        if skills.is_empty() {
            return None;
        }
        let mut description = format!("{INTRO}\n{index}");
        if left_out > 0 {
            description = format!("{description}Skills left out of this list: {left_out}.\n");
        }
        Some(Self {
            skills,
            description,
        })
    }

    /// The tool description: what the tool does, then the index of skills.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The names listed in the index, sorted.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.skills.keys().map(String::as_str).collect()
    }
}

/// One index entry: the description on a single line, cut to its cap.
fn index_line(name: &str, description: &str) -> String {
    let words = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let shown = if words.chars().count() > MAX_INDEX_DESCRIPTION_CHARS {
        let cut: String = words.chars().take(MAX_INDEX_DESCRIPTION_CHARS).collect();
        format!("{cut}{ELLIPSIS}")
    } else {
        words
    };
    format!("- {name}: {shown}\n")
}

impl Tool for SkillTool {
    fn name(&self) -> &'static str {
        SKILL_TOOL_NAME
    }

    fn call(&self, input: &Value) -> Result<ToolOutput, ToolError> {
        let invalid = |reason: String| ToolError::InvalidInput {
            tool: SKILL_TOOL_NAME,
            reason,
        };
        let Args { name } = Args::deserialize(input).map_err(|e| invalid(e.to_string()))?;
        let Some(skill) = self.skills.get(&name) else {
            return Err(invalid(format!(
                "unknown skill `{name}`; the skills are: {}",
                self.names().join(", ")
            )));
        };
        let mut text = format!("Skill `{name}` ({}):\n\n", skill.source.as_str());
        let mut end = skill.body.len().min(MAX_SKILL_BODY_BYTES);
        while !skill.body.is_char_boundary(end) {
            end -= 1;
        }
        text.push_str(&skill.body[..end]);
        let truncated = end < skill.body.len();
        if truncated {
            text = format!("{text}\n[skill cut at {MAX_SKILL_BODY_BYTES} bytes]");
        }
        Ok(ToolOutput { text, truncated })
    }
}
