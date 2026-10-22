// SPDX-License-Identifier: AGPL-3.0-only

//! Loads definitions from the user's config directory and the project, and
//! merges them under the trust rules.
//!
//! Project files are untrusted prompt input. They load only when the digest of
//! the exact bytes read matches the digest the user trusted, which only the
//! store-backed [`Catalog::load_trusted`] can supply; otherwise they are listed
//! as pending and never returned. A project file may replace the description
//! and prompt of a user or built-in definition, and may narrow an agent's
//! tools, but never widen them.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use coda_store::{Store, StoreError};
use tracing::{debug, warn};

use crate::builtin;
use crate::definition::{
    AgentDef, CommandDef, Issue, SkillDef, parse_agent, parse_command, parse_skill,
};
use crate::digest::digest_of;

/// Directory inside a repository that holds project definitions.
pub const PROJECT_DIR: &str = ".coda";
pub(crate) const AGENTS_DIR: &str = "agents";
pub(crate) const SKILLS_DIR: &str = "skills";
pub(crate) const COMMANDS_DIR: &str = "commands";
const SYMLINK_MESSAGE: &str = "it is a symlink, which a project cannot use to point outside itself";
const EXTENSION: &str = "md";
/// A definition larger than this is refused: prompts are small, and a project
/// file is untrusted.
const MAX_DEFINITION_BYTES: u64 = 64 * 1024;

/// Why a project file is not in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingReason {
    NotTrusted,
    ChangedSinceTrust,
}

impl PendingReason {
    /// A phrase for the user.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotTrusted => "the repository is not trusted",
            Self::ChangedSinceTrust => "project definitions changed since they were trusted",
        }
    }
}

/// Where a definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Ships in the binary.
    Builtin,
    User,
    Project,
}

impl Source {
    /// The word shown to the user.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "built-in",
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

    pub(crate) const fn folder(self) -> &'static str {
        match self {
            Self::Agent => AGENTS_DIR,
            Self::Skill => SKILLS_DIR,
            Self::Command => COMMANDS_DIR,
        }
    }

    const ALL: [Self; 3] = [Self::Agent, Self::Skill, Self::Command];
}

/// A loaded definition and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry<T> {
    pub def: T,
    pub source: Source,
    pub path: PathBuf,
}

/// A project file that is not in use, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub kind: Kind,
    pub path: PathBuf,
    pub reason: PendingReason,
}

impl Pending {
    /// The name the file would define: its file name without `.md`. The file
    /// is not parsed, so the name is unchecked text from an untrusted project.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.path.file_stem().and_then(|stem| stem.to_str())
    }
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
    pub(crate) fn new(path: &Path, field: &str, message: impl Into<String>) -> Self {
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

/// A definition file read once; its digest and its parse use these bytes.
#[derive(Debug)]
pub(crate) struct DefinitionFile {
    pub kind: Kind,
    pub stem: String,
    pub path: PathBuf,
    pub text: String,
}

/// Everything read from one definitions directory.
#[derive(Debug, Default)]
pub(crate) struct Snapshot {
    pub entries: Vec<(Kind, PathBuf)>,
    pub files: Vec<DefinitionFile>,
    pub problems: Vec<LoadProblem>,
}

impl Snapshot {
    /// Lists and reads the `.md` files under `dir/{agents,skills,commands}`.
    pub fn take(dir: &Path) -> Self {
        let mut snapshot = Self::default();
        for kind in Kind::ALL {
            let sub_dir = dir.join(kind.folder());
            let paths = match list_markdown(&sub_dir) {
                Ok(paths) => paths,
                Err(e) => {
                    snapshot
                        .problems
                        .push(LoadProblem::new(&sub_dir, "directory", e.to_string()));
                    continue;
                }
            };
            for path in paths {
                snapshot.entries.push((kind, path.clone()));
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned)
                else {
                    snapshot.problems.push(LoadProblem::new(
                        &path,
                        "file name",
                        "the name is not valid UTF-8",
                    ));
                    continue;
                };
                match read_definition(&path) {
                    Ok(text) => snapshot.files.push(DefinitionFile {
                        kind,
                        stem,
                        path,
                        text,
                    }),
                    Err(message) => snapshot
                        .problems
                        .push(LoadProblem::new(&path, "file", message)),
                }
            }
        }
        snapshot
    }

    pub fn digest(&self) -> String {
        digest_of(&self.files, &self.problems)
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

/// Which project files may load.
#[derive(Clone, Copy)]
enum Admit<'a> {
    /// Those whose digest equals the stored one, if any.
    Trusted(Option<&'a str>),
    /// All of them; only for reporting problems, never for use.
    Everything,
}

impl Catalog {
    /// Loads `agents/`, `skills/` and `commands/` from `user_dir`. The files
    /// under `<project_root>/.coda/` are listed as pending and not loaded. A
    /// file that does not parse is recorded in [`Catalog::problems`] and skipped.
    #[must_use]
    pub fn load(user_dir: &Path, project_root: Option<&Path>) -> Self {
        Self::load_with(user_dir, project_root, Admit::Trusted(None))
    }

    /// Like [`Catalog::load`], but project files load when the user has trusted
    /// the repository and the bytes read now still match what they trusted.
    ///
    /// # Errors
    /// Returns [`StoreError`] when the trust decision cannot be read.
    pub fn load_trusted(
        store: &Store,
        user_dir: &Path,
        project_root: Option<&Path>,
    ) -> Result<Self, StoreError> {
        let stored = match project_root {
            Some(root) => store.project_trust_digest(&repo_key(root))?,
            None => None,
        };
        Ok(Self::load_with(
            user_dir,
            project_root,
            Admit::Trusted(stored.as_deref()),
        ))
    }

    /// Every problem in the user and project files, trusted or not. Linting
    /// reads the files but returns no definition, so nothing from an untrusted
    /// project can be used through it.
    #[must_use]
    pub fn check_all(user_dir: &Path, project_root: Option<&Path>) -> Vec<LoadProblem> {
        Self::load_with(user_dir, project_root, Admit::Everything).problems
    }

    /// Only the agents that ship in the binary.
    #[must_use]
    pub fn builtin() -> Self {
        let mut catalog = Self::default();
        catalog.absorb(builtin::snapshot(), Source::Builtin);
        catalog
    }

    fn load_with(user_dir: &Path, project_root: Option<&Path>, admit: Admit<'_>) -> Self {
        let mut catalog = Self::builtin();
        debug!(dir = %user_dir.display(), "loading user definitions");
        catalog.absorb(Snapshot::take(user_dir), Source::User);
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
        debug!(dir = %project_dir.display(), "reading project definitions");
        catalog.apply_project(Snapshot::take(&project_dir), admit);
        catalog
    }

    /// Admits `snapshot` when `admit` allows it, else lists its files as pending.
    /// The digest is taken from the snapshot itself, so what is compared is
    /// exactly what is parsed.
    fn apply_project(&mut self, snapshot: Snapshot, admit: Admit<'_>) {
        let reason = match admit {
            Admit::Everything => None,
            Admit::Trusted(Some(stored)) if stored == snapshot.digest() => None,
            Admit::Trusted(Some(_)) => Some(PendingReason::ChangedSinceTrust),
            Admit::Trusted(None) => Some(PendingReason::NotTrusted),
        };
        match reason {
            None => self.absorb(snapshot, Source::Project),
            Some(reason) => {
                self.problems.extend(
                    snapshot
                        .problems
                        .into_iter()
                        .filter(|p| p.field == "directory"),
                );
                self.pending
                    .extend(snapshot.entries.into_iter().map(|(kind, path)| Pending {
                        kind,
                        path,
                        reason,
                    }));
            }
        }
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

    /// Project files that are not in use.
    #[must_use]
    pub fn pending(&self) -> &[Pending] {
        &self.pending
    }

    /// Files and fields that were skipped or ignored.
    #[must_use]
    pub fn problems(&self) -> &[LoadProblem] {
        &self.problems
    }

    fn absorb(&mut self, snapshot: Snapshot, source: Source) {
        self.problems.extend(snapshot.problems);
        for file in snapshot.files {
            let DefinitionFile {
                kind,
                stem,
                path,
                text,
            } = file;
            match kind {
                Kind::Agent => match parse_agent(&text, &stem) {
                    Ok(def) => self.add_agent(stem, Entry { def, source, path }),
                    Err(issues) => self.reject(&path, issues),
                },
                Kind::Skill => match parse_skill(&text, &stem) {
                    Ok(def) => {
                        self.skills.insert(stem, Entry { def, source, path });
                    }
                    Err(issues) => self.reject(&path, issues),
                },
                Kind::Command => match parse_command(&text, &stem) {
                    Ok(def) => {
                        self.commands.insert(stem, Entry { def, source, path });
                    }
                    Err(issues) => self.reject(&path, issues),
                },
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
                    "a project file cannot add tools to a built-in or user agent; ignored: {}",
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

    fn reject(&mut self, path: &Path, issues: Vec<Issue>) {
        warn!(path = %path.display(), "skipping an invalid definition file");
        self.problems.extend(LoadProblem::from_issues(path, issues));
    }
}

/// Records that the user trusts the project definitions of `project_root` as
/// they are now. Later loads use them only while their bytes still match.
///
/// # Errors
/// Returns [`StoreError`] when the decision cannot be written.
pub fn trust_project(store: &mut Store, project_root: &Path) -> Result<(), StoreError> {
    let snapshot = Snapshot::take(&project_root.join(PROJECT_DIR));
    store.trust_project(&repo_key(project_root), &snapshot.digest())
}

/// Removes the trust decision of `project_root`; true when there was one.
///
/// # Errors
/// Returns [`StoreError`] when the decision cannot be removed.
pub fn revoke_project(store: &mut Store, project_root: &Path) -> Result<bool, StoreError> {
    store.revoke_project_trust(&repo_key(project_root))
}

fn repo_key(project_root: &Path) -> String {
    project_root.display().to_string()
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
/// cannot point a definition at a file outside it. The read is capped too, in
/// case the file grew after it was measured.
fn read_definition(path: &Path) -> Result<String, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("it is not a regular file".to_owned());
    }
    let too_large = || format!("it is larger than {MAX_DEFINITION_BYTES} bytes");
    if meta.len() > MAX_DEFINITION_BYTES {
        return Err(too_large());
    }
    let mut text = String::new();
    File::open(path)
        .and_then(|file| {
            file.take(MAX_DEFINITION_BYTES + 1)
                .read_to_string(&mut text)
        })
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_DEFINITION_BYTES {
        return Err(too_large());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn agent(prompt: &str) -> String {
        format!(
            "+++\nname = \"scout\"\n\
             description = \"Scouts. Use when asked. Do not use otherwise.\"\n\
             tier = \"fast\"\neffort = \"low\"\ntools = [\"read\"]\n+++\n{prompt}\n"
        )
    }

    #[test]
    fn an_edit_after_the_read_cannot_change_what_a_matching_digest_admits() {
        // Arrange
        let repo = TempDir::new().unwrap();
        let file = repo.path().join(".coda/agents/scout.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, agent("reviewed prompt")).unwrap();
        let snapshot = Snapshot::take(&repo.path().join(PROJECT_DIR));
        let trusted = snapshot.digest();
        fs::write(&file, agent("swapped in after the check")).unwrap();

        // Act
        let mut catalog = Catalog::default();
        catalog.apply_project(snapshot, Admit::Trusted(Some(&trusted)));

        // Assert
        let scout = catalog.agent("scout").unwrap();
        assert_eq!(scout.def.body.trim(), "reviewed prompt");
    }

    #[test]
    fn a_digest_of_other_bytes_admits_nothing() {
        // Arrange
        let repo = TempDir::new().unwrap();
        let file = repo.path().join(".coda/agents/scout.md");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, agent("reviewed prompt")).unwrap();
        let trusted = Snapshot::take(&repo.path().join(PROJECT_DIR)).digest();
        fs::write(&file, agent("swapped in before the read")).unwrap();

        // Act
        let mut catalog = Catalog::default();
        catalog.apply_project(
            Snapshot::take(&repo.path().join(PROJECT_DIR)),
            Admit::Trusted(Some(&trusted)),
        );

        // Assert
        assert!(catalog.agent("scout").is_none());
        assert_eq!(
            catalog.pending()[0].reason,
            PendingReason::ChangedSinceTrust
        );
    }
}
