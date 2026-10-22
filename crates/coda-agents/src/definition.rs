// SPDX-License-Identifier: AGPL-3.0-only

//! The agent, skill and command formats and their checks.
//!
//! Every parse collects all the problems of a file, each naming the
//! frontmatter field at fault, so a lint can report them in one pass.

use std::fmt;
use std::str::FromStr;

use coda_config::routing::Tier;
use coda_tools::{TOOL_NAMES, task};
use serde::Deserialize;

use crate::frontmatter;

/// Frontmatter keys that would declare a hook. Hooks load only from the user's
/// config, so a definition file may not carry one.
const HOOK_KEYS: [&str; 7] = [
    "hook",
    "hooks",
    "pre_tool",
    "post_tool",
    "pre_tool_use",
    "post_tool_use",
    "session_start",
];
const MODEL_KEY: &str = "model";
const FRONTMATTER_FIELD: &str = "frontmatter";
const USE_WHEN: &str = "use when";
const DO_NOT_USE: &str = "do not use";

/// One thing wrong with a definition file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// The frontmatter field at fault, or `frontmatter` for the file as a whole.
    pub field: String,
    /// What is wrong, in plain words.
    pub message: String,
}

impl Issue {
    fn new(field: &str, message: impl Into<String>) -> Self {
        Self {
            field: field.to_owned(),
            message: message.into(),
        }
    }
}

/// How hard an agent should think.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    /// Every effort level, lowest first.
    pub const ALL: [Self; 3] = [Self::Low, Self::Medium, Self::High];

    /// The name used in agent files.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        Self::ALL.into_iter().find(|e| e.as_str() == s).ok_or(())
    }
}

/// A sub-agent: a prompt, the tools it may call and the model tier it runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    pub name: String,
    /// Says when to use the agent and when not to; routing reads it.
    pub description: String,
    pub tier: Tier,
    pub effort: Effort,
    /// The only tools the agent can call. An agent is read-only because this
    /// list holds no write tool.
    pub tools: Vec<String>,
    /// The agent's system prompt.
    pub body: String,
}

/// A body of instructions loaded on request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// A prompt template run as a slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDef {
    pub name: String,
    pub description: String,
    /// Shown after the command name, for example `<issue>`.
    pub argument_hint: Option<String>,
    pub body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    name: String,
    description: String,
    tier: String,
    effort: String,
    tools: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSkill {
    name: String,
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    name: String,
    description: String,
    #[serde(default)]
    argument_hint: Option<String>,
}

/// Parses an agent file. `stem` is the file name without `.md`; the `name`
/// field must equal it.
///
/// # Errors
/// Returns every [`Issue`] found: a missing, unknown or hook key, a model id,
/// an unknown tier, effort or tool, or a description that does not say when
/// to use the agent and when not to.
pub fn parse_agent(text: &str, stem: &str) -> Result<AgentDef, Vec<Issue>> {
    let (mut table, body) = split_table(text)?;
    let mut issues = forbidden_keys(&mut table);
    if let Some(issue) = take_model_key(&mut table) {
        issues.push(issue);
    }
    let raw: RawAgent = deserialize(table, &mut issues)?;
    issues.extend(check_name(&raw.name, stem));
    issues.extend(check_description(&raw.description));
    let tier = raw.tier.parse::<Tier>();
    if tier.is_err() {
        let known: Vec<&str> = Tier::ALL.iter().map(|t| t.as_str()).collect();
        issues.push(Issue::new(
            "tier",
            format!(
                "unknown tier `{}`; use one of {}",
                raw.tier,
                known.join(", ")
            ),
        ));
    }
    let effort = raw.effort.parse::<Effort>();
    if effort.is_err() {
        let known: Vec<&str> = Effort::ALL.iter().map(|e| e.as_str()).collect();
        issues.push(Issue::new(
            "effort",
            format!(
                "unknown effort `{}`; use one of {}",
                raw.effort,
                known.join(", ")
            ),
        ));
    }
    for tool in raw
        .tools
        .iter()
        .filter(|t| !TOOL_NAMES.contains(&t.as_str()))
    {
        let message = if tool == task::NAME {
            format!("`{tool}` cannot be allowed: sub-agents cannot start sub-agents")
        } else {
            format!("unknown tool `{tool}`")
        };
        issues.push(Issue::new("tools", message));
    }
    let (Ok(tier), Ok(effort)) = (tier, effort) else {
        return Err(issues);
    };
    finish(
        issues,
        AgentDef {
            name: raw.name,
            description: raw.description,
            tier,
            effort,
            tools: raw.tools,
            body: body.to_owned(),
        },
    )
}

/// Parses a skill file.
///
/// # Errors
/// Returns every [`Issue`] found, as for [`parse_agent`].
pub fn parse_skill(text: &str, stem: &str) -> Result<SkillDef, Vec<Issue>> {
    let (mut table, body) = split_table(text)?;
    let mut issues = forbidden_keys(&mut table);
    let raw: RawSkill = deserialize(table, &mut issues)?;
    issues.extend(check_name(&raw.name, stem));
    finish(
        issues,
        SkillDef {
            name: raw.name,
            description: raw.description,
            body: body.to_owned(),
        },
    )
}

/// Parses a command file.
///
/// # Errors
/// Returns every [`Issue`] found, as for [`parse_agent`].
pub fn parse_command(text: &str, stem: &str) -> Result<CommandDef, Vec<Issue>> {
    let (mut table, body) = split_table(text)?;
    let mut issues = forbidden_keys(&mut table);
    let raw: RawCommand = deserialize(table, &mut issues)?;
    issues.extend(check_name(&raw.name, stem));
    finish(
        issues,
        CommandDef {
            name: raw.name,
            description: raw.description,
            argument_hint: raw.argument_hint,
            body: body.to_owned(),
        },
    )
}

fn finish<T>(issues: Vec<Issue>, def: T) -> Result<T, Vec<Issue>> {
    if issues.is_empty() {
        Ok(def)
    } else {
        Err(issues)
    }
}

fn split_table(text: &str) -> Result<(toml::Table, &str), Vec<Issue>> {
    let (front, body) = frontmatter::split(text).ok_or_else(|| {
        vec![Issue::new(
            FRONTMATTER_FIELD,
            "the file must start with a `+++` line, hold TOML, and close it with another `+++` line",
        )]
    })?;
    let table = front
        .parse::<toml::Table>()
        .map_err(|e| vec![Issue::new(FRONTMATTER_FIELD, e.message().to_owned())])?;
    Ok((table, body))
}

/// Removes hook keys from `table` and reports each, so they are not also
/// reported as unknown keys.
fn forbidden_keys(table: &mut toml::Table) -> Vec<Issue> {
    HOOK_KEYS
        .iter()
        .filter(|key| table.remove(**key).is_some())
        .map(|key| {
            Issue::new(
                key,
                "hooks load only from the user's config, never from a definition file",
            )
        })
        .collect()
}

fn take_model_key(table: &mut toml::Table) -> Option<Issue> {
    table.remove(MODEL_KEY).map(|_| {
        Issue::new(
            MODEL_KEY,
            "agent files name a tier, never a model id; map tiers to models in config.toml",
        )
    })
}

/// Deserializes `table`. A structural failure ends the parse with the issues
/// collected so far plus that failure.
fn deserialize<T: serde::de::DeserializeOwned>(
    table: toml::Table,
    issues: &mut Vec<Issue>,
) -> Result<T, Vec<Issue>> {
    T::deserialize(table).map_err(|e| {
        let mut all = std::mem::take(issues);
        all.push(Issue::new(FRONTMATTER_FIELD, e.message().to_owned()));
        all
    })
}

fn check_name(name: &str, stem: &str) -> Option<Issue> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid {
        return Some(Issue::new(
            "name",
            "use lowercase letters, digits and hyphens only",
        ));
    }
    (name != stem).then(|| {
        Issue::new(
            "name",
            format!("`{name}` must match the file name `{stem}`"),
        )
    })
}

fn check_description(description: &str) -> Option<Issue> {
    let lower = description.to_lowercase();
    let says_when_not = lower.contains(DO_NOT_USE);
    // "Do not use when" also contains "use when"; it must not count for both.
    let says_when = lower.replace(DO_NOT_USE, "").contains(USE_WHEN);
    let missing = match (says_when, says_when_not) {
        (true, true) => return None,
        (false, true) => "when to use the agent (\"Use when ...\")",
        (true, false) => "when not to use the agent (\"Do not use ...\")",
        (false, false) => "when to use the agent and when not to",
    };
    Some(Issue::new("description", format!("it must say {missing}")))
}
