// SPDX-License-Identifier: AGPL-3.0-only

//! Loads definitions from the user's config directory and the project, and
//! merges them under the trust rules.
//!
//! Project files are untrusted prompt input, so for now they are only listed
//! as pending and never returned. Once a project file is allowed to load, it
//! may replace the description and prompt of a user definition and may narrow
//! an agent's tools, but never widen them.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::definition::{
    AgentDef, CommandDef, Issue, SkillDef, parse_agent, parse_command, parse_skill,
};

/// Directory inside a repository that holds project definitions.
pub const PROJECT_DIR: &str = ".coda";
const AGENTS_DIR: &str = "agents";
const SKILLS_DIR: &str = "skills";
const COMMANDS_DIR: &str = "commands";
const SYMLINK_MESSAGE: &str = "it is a symlink, which a project cannot use to point outside itself";
const EXTENSION: &str = "md";
/// A definition larger than this is refused: prompts are small, and a project
/// file is untrusted.
const MAX_DEFINITION_BYTES: u64 = 64 * 1024;

/// Whether a project's definitions may load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trust {
    /// Only tests construct this until the user can trust a repository.
    #[cfg_attr(not(test), allow(dead_code))]
    Trusted,
    Untrusted,
}

/// Where a definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    User,
    Project,
}

impl Source {
    /// The word shown to the user.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
        }
    }
}

/// What a definition file defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Agent,
    Skill,
    Command,
}

impl Kind {
    /// The singular noun, as shown to the user.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Skill => "skill",
            Self::Command => "command",
        }
    }
}

/// A loaded definition and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<T> {
    pub def: T,
    pub source: Source,
    pub path: PathBuf,
}

/// A project file waiting for the user's trust decision. It has not been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub kind: Kind,
    pub path: PathBuf,
}

/// A file that was skipped, or part of one that was ignored, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{}: {field}: {message}", path.display())]
pub struct LoadProblem {
    pub path: PathBuf,
    pub field: String,
    pub message: String,
}

impl LoadProblem {
    fn new(path: &Path, field: &str, message: impl Into<String>) -> Self {
        Self {
            path: path.to_path_buf(),
            field: field.to_owned(),
            message: message.into(),
        }
    }

    fn from_issues(path: &Path, issues: Vec<Issue>) -> Vec<Self> {
        issues
            .into_iter()
            .map(|i| Self::new(path, &i.field, i.message))
            .collect()
    }
}

/// Every usable definition, by name, plus what was held back or skipped.
#[derive(Debug, Default)]
pub struct Catalog {
    agents: BTreeMap<String, Entry<AgentDef>>,
    skills: BTreeMap<String, Entry<SkillDef>>,
    commands: BTreeMap<String, Entry<CommandDef>>,
    pending: Vec<Pending>,
    problems: Vec<LoadProblem>,
}

impl Catalog {
    /// Loads `agents/`, `skills/` and `commands/` from `user_dir`. The files
    /// under `<project_root>/.coda/` are listed as pending and not loaded. A
    /// file that does not parse is recorded in [`Catalog::problems`] and skipped.
    #[must_use]
    pub fn load(user_dir: &Path, project_root: Option<&Path>) -> Self {
        Self::load_with(user_dir, project_root, Trust::Untrusted)
    }

    fn load_with(user_dir: &Path, project_root: Option<&Path>, trust: Trust) -> Self {
        let mut catalog = Self::default();
        catalog.load_source(user_dir, Source::User);
        let Some(root) = project_root else {
            return catalog;
        };
        let project_dir = root.join(PROJECT_DIR);
        if is_symlink(&project_dir) {
            catalog
                .problems
                .push(LoadProblem::new(&project_dir, "directory", SYMLINK_MESSAGE));
            return catalog;
        }
        match trust {
            Trust::Trusted => catalog.load_source(&project_dir, Source::Project),
            Trust::Untrusted => catalog.list_pending(&project_dir),
        }
        catalog
    }

    #[must_use]
    pub fn agent(&self, name: &str) -> Option<&Entry<AgentDef>> {
        self.agents.get(name)
    }

    pub fn agents(&self) -> impl Iterator<Item = &Entry<AgentDef>> {
        self.agents.values()
    }

    #[must_use]
    pub fn skill(&self, name: &str) -> Option<&Entry<SkillDef>> {
        self.skills.get(name)
    }

    pub fn skills(&self) -> impl Iterator<Item = &Entry<SkillDef>> {
        self.skills.values()
    }

    #[must_use]
    pub fn command(&self, name: &str) -> Option<&Entry<CommandDef>> {
        self.commands.get(name)
    }

    pub fn commands(&self) -> impl Iterator<Item = &Entry<CommandDef>> {
        self.commands.values()
    }

    /// Project files held back until the repository is trusted.
    #[must_use]
    pub fn pending(&self) -> &[Pending] {
        &self.pending
    }

    /// Files and fields that were skipped or ignored.
    #[must_use]
    pub fn problems(&self) -> &[LoadProblem] {
        &self.problems
    }

    fn load_source(&mut self, dir: &Path, source: Source) {
        debug!(dir = %dir.display(), ?source, "loading definitions");
        for (stem, path, text) in self.read_files(&dir.join(AGENTS_DIR)) {
            match parse_agent(&text, &stem) {
                Ok(def) => self.add_agent(stem, Entry { def, source, path }),
                Err(issues) => self.reject(&path, issues),
            }
        }
        for (stem, path, text) in self.read_files(&dir.join(SKILLS_DIR)) {
            match parse_skill(&text, &stem) {
                Ok(def) => {
                    self.skills.insert(stem, Entry { def, source, path });
                }
                Err(issues) => self.reject(&path, issues),
            }
        }
        for (stem, path, text) in self.read_files(&dir.join(COMMANDS_DIR)) {
            match parse_command(&text, &stem) {
                Ok(def) => {
                    self.commands.insert(stem, Entry { def, source, path });
                }
                Err(issues) => self.reject(&path, issues),
            }
        }
    }

    fn add_agent(&mut self, stem: String, incoming: Entry<AgentDef>) {
        let Some(user) = self
            .agents
            .get(&stem)
            .filter(|_| incoming.source == Source::Project)
        else {
            self.agents.insert(stem, incoming);
            return;
        };
        let widened: Vec<&String> = incoming
            .def
            .tools
            .iter()
            .filter(|tool| !user.def.tools.contains(tool))
            .collect();
        if !widened.is_empty() {
            let names: Vec<&str> = widened.iter().map(|t| t.as_str()).collect();
            warn!(agent = %stem, "project agent asked for tools the user definition lacks");
            self.problems.push(LoadProblem::new(
                &incoming.path,
                "tools",
                format!(
                    "a project file cannot add tools to a user agent; ignored: {}",
                    names.join(", ")
                ),
            ));
        }
        let mut merged = user.def.clone();
        merged
            .tools
            .retain(|tool| incoming.def.tools.contains(tool));
        merged.description = incoming.def.description;
        merged.body = incoming.def.body;
        self.agents.insert(
            stem,
            Entry {
                def: merged,
                source: Source::Project,
                path: incoming.path,
            },
        );
    }

    fn list_pending(&mut self, project_dir: &Path) {
        for (kind, sub) in [
            (Kind::Agent, AGENTS_DIR),
            (Kind::Skill, SKILLS_DIR),
            (Kind::Command, COMMANDS_DIR),
        ] {
            match list_markdown(&project_dir.join(sub)) {
                Ok(paths) => self
                    .pending
                    .extend(paths.into_iter().map(|path| Pending { kind, path })),
                Err(e) => self.problems.push(LoadProblem::new(
                    &project_dir.join(sub),
                    "directory",
                    e.to_string(),
                )),
            }
        }
    }

    fn reject(&mut self, path: &Path, issues: Vec<Issue>) {
        warn!(path = %path.display(), "skipping an invalid definition file");
        self.problems.extend(LoadProblem::from_issues(path, issues));
    }

    /// Reads every readable definition file in `dir` as `(stem, path, text)`.
    fn read_files(&mut self, dir: &Path) -> Vec<(String, PathBuf, String)> {
        let paths = match list_markdown(dir) {
            Ok(paths) => paths,
            Err(e) => {
                self.problems
                    .push(LoadProblem::new(dir, "directory", e.to_string()));
                return Vec::new();
            }
        };
        let mut files = Vec::new();
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                self.problems.push(LoadProblem::new(
                    &path,
                    "file name",
                    "the name is not valid UTF-8",
                ));
                continue;
            };
            match read_definition(&path) {
                Ok(text) => files.push((stem, path, text)),
                Err(message) => self.problems.push(LoadProblem::new(&path, "file", message)),
            }
        }
        files
    }
}

/// The `.md` files of `dir`, sorted. A missing directory holds none and a
/// symlinked one is refused.
fn list_markdown(dir: &Path) -> io::Result<Vec<PathBuf>> {
    if is_symlink(dir) {
        return Err(io::Error::other(SYMLINK_MESSAGE));
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == EXTENSION) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink())
}

/// Reads a regular file of bounded size. A symlink is refused so a project
/// cannot point a definition at a file outside it.
fn read_definition(path: &Path) -> Result<String, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("it is not a regular file".to_owned());
    }
    if meta.len() > MAX_DEFINITION_BYTES {
        return Err(format!("it is larger than {MAX_DEFINITION_BYTES} bytes"));
    }
    fs::read_to_string(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    fn agent(name: &str, prompt: &str, tools: &str) -> String {
        format!(
            "+++\nname = \"{name}\"\n\
             description = \"Does {name}. Use when asked. Do not use otherwise.\"\n\
             tier = \"fast\"\neffort = \"low\"\ntools = [{tools}]\n+++\n{prompt}\n"
        )
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    struct Fixture {
        user: TempDir,
        repo: TempDir,
    }

    fn fixture() -> Fixture {
        Fixture {
            user: TempDir::new().unwrap(),
            repo: TempDir::new().unwrap(),
        }
    }

    impl Fixture {
        fn load(&self, trust: Trust) -> Catalog {
            Catalog::load_with(self.user.path(), Some(self.repo.path()), trust)
        }
    }

    #[test]
    fn a_project_agent_overrides_the_user_prompt_but_cannot_widen_tools() {
        // Arrange
        let f = fixture();
        write(
            f.user.path(),
            "agents/scout.md",
            &agent("scout", "user prompt", "\"read\""),
        );
        write(
            f.repo.path(),
            ".coda/agents/scout.md",
            &agent("scout", "project prompt", "\"read\", \"bash\""),
        );

        // Act
        let catalog = f.load(Trust::Trusted);

        // Assert
        let scout = catalog.agent("scout").unwrap();
        assert_eq!(scout.def.body.trim(), "project prompt");
        assert_eq!(scout.def.tools, ["read"]);
        assert_eq!(scout.source, Source::Project);
        assert!(
            catalog
                .problems()
                .iter()
                .any(|p| p.to_string().contains("bash"))
        );
    }

    #[test]
    fn a_project_agent_may_narrow_the_tools_of_a_user_agent() {
        // Arrange
        let f = fixture();
        write(
            f.user.path(),
            "agents/scout.md",
            &agent("scout", "u", "\"read\", \"grep\""),
        );
        write(
            f.repo.path(),
            ".coda/agents/scout.md",
            &agent("scout", "p", "\"grep\""),
        );

        // Act
        let catalog = f.load(Trust::Trusted);

        // Assert
        assert_eq!(catalog.agent("scout").unwrap().def.tools, ["grep"]);
    }

    #[test]
    fn a_project_only_agent_is_usable_once_trusted() {
        // Arrange
        let f = fixture();
        write(
            f.repo.path(),
            ".coda/agents/local.md",
            &agent("local", "p", "\"read\""),
        );

        // Act
        let catalog = f.load(Trust::Trusted);

        // Assert
        assert_eq!(catalog.agent("local").unwrap().source, Source::Project);
    }

    #[test]
    fn a_hook_key_in_a_project_file_is_rejected_and_the_file_is_not_loaded() {
        // Arrange
        let f = fixture();
        let text = agent("local", "p", "\"read\"").replace("+++\np", "hooks = [\"x\"]\n+++\np");
        write(f.repo.path(), ".coda/agents/local.md", &text);

        // Act
        let catalog = f.load(Trust::Trusted);

        // Assert
        assert!(catalog.agent("local").is_none());
        assert!(
            catalog
                .problems()
                .iter()
                .any(|p| p.to_string().contains("hooks"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_coda_directory_is_refused() {
        // Arrange
        let f = fixture();
        let elsewhere = TempDir::new().unwrap();
        write(
            elsewhere.path(),
            "agents/local.md",
            &agent("local", "p", "\"read\""),
        );
        std::os::unix::fs::symlink(elsewhere.path(), f.repo.path().join(PROJECT_DIR)).unwrap();

        // Act
        let catalog = f.load(Trust::Trusted);

        // Assert
        assert!(catalog.agent("local").is_none());
        assert!(
            catalog
                .problems()
                .iter()
                .any(|p| p.message.contains("symlink"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_subfolder_is_refused() {
        // Arrange
        let f = fixture();
        let elsewhere = TempDir::new().unwrap();
        write(
            elsewhere.path(),
            "local.md",
            &agent("local", "p", "\"read\""),
        );
        fs::create_dir(f.repo.path().join(PROJECT_DIR)).unwrap();
        std::os::unix::fs::symlink(
            elsewhere.path(),
            f.repo.path().join(PROJECT_DIR).join(AGENTS_DIR),
        )
        .unwrap();

        // Act
        let catalog = f.load(Trust::Trusted);

        // Assert
        assert!(catalog.agent("local").is_none());
        assert!(
            catalog
                .problems()
                .iter()
                .any(|p| p.message.contains("symlink"))
        );
    }
}
