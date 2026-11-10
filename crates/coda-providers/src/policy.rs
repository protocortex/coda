// SPDX-License-Identifier: Apache-2.0

//! The pure permission decision and its decision-table fixture.

use std::path::{Component, Path, PathBuf};

use coda_config::config::PermissionMode;
use coda_protocol::paths::{Target, resolve_in_workspace};

use crate::tool_request::Tool;

/// File names that change what a build, install or task runner executes, at
/// any depth, matched without regard to case because the usual macOS
/// filesystem does not tell `Makefile` from `makefile`. `edit`, `write` and `multi_edit` all ask about these in
/// accept-edits mode.
const EXECUTION_FILE_NAMES: [&str; 28] = [
    ".gitattributes",
    "Cargo.toml",
    "build.rs",
    "rust-toolchain.toml",
    "Makefile",
    "GNUmakefile",
    "justfile",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "tox.ini",
    "noxfile.py",
    "conftest.py",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "pom.xml",
    "CMakeLists.txt",
    "Rakefile",
    "Gemfile",
    "Dockerfile",
    "Taskfile.yml",
    "Taskfile.yaml",
    ".envrc",
    ".npmrc",
    ".pre-commit-config.yaml",
];
const CARGO_DIR: &str = ".cargo";
const CARGO_CONFIG_FILE: &str = "config.toml";
/// Directories whose contents run on commits, in CI or by hand, from the root.
const ROOT_EXECUTION_DIRS: [&[&str]; 2] = [&[".github", "workflows"], &["scripts"]];
/// Git's own directory, or the `.git` file a worktree uses to point at it. Its
/// `config` and `hooks` run code on the next git command and `info` picks the
/// drivers `config` defines, so any path
/// through a component with this name, at any depth, is execution affecting.
const GIT_DIR: &str = ".git";

/// What the policy says to do with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Run it.
    Allow,
    /// Refuse it.
    Deny,
    /// Ask a user; the caller denies it if nobody answers in time.
    Prompt,
}

/// One target path with its [`classify_target`](coda_protocol::paths::classify_target) result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedTarget {
    /// The path as the backend gave it.
    pub path: PathBuf,
    /// Where it lands relative to the workspace.
    pub target: Target,
}

/// Everything [`decide`] looks at.
#[derive(Debug, Clone, Copy)]
pub struct PolicyInput<'a> {
    /// The configured answering policy.
    pub mode: PermissionMode,
    /// The requested tool.
    pub tool: &'a Tool,
    /// The workspace root, to name paths relative to it.
    pub workspace_root: &'a Path,
    /// The request's paths, already classified.
    pub targets: &'a [ClassifiedTarget],
    /// Whether an exact stored grant matches this tool, input and cwd.
    pub granted: bool,
}

/// Applies the decision table: first matching rule wins.
#[must_use]
pub fn decide(input: &PolicyInput<'_>) -> Verdict {
    if input.mode == PermissionMode::DenyAll {
        return Verdict::Deny;
    }
    match input.tool {
        // Skill reads only the definitions the user already admitted for the session.
        // The git tools run a fixed read-only command that ignores the repository's own programs.
        // Recall reads only what sessions of this repository stored and sends it nowhere itself.
        Tool::Retrieve
        | Tool::CodeSearch
        | Tool::CodeSymbol
        | Tool::CodeOutline
        | Tool::Skill
        | Tool::GitStatus
        | Tool::GitDiff
        | Tool::Recall => Verdict::Allow,
        Tool::Unknown(_) => Verdict::Prompt,
        // A fetch reaches the network, so like a command it needs a grant for its exact address.
        Tool::Bash | Tool::Fetch => grant_or_prompt(input.granted),
        Tool::Read | Tool::Grep | Tool::List | Tool::CodeReadRange => {
            if all_inside(input.targets) {
                Verdict::Allow
            } else {
                Verdict::Deny
            }
        }
        Tool::Edit | Tool::Write | Tool::MultiEdit => decide_edit(input),
    }
}

/// A path tool with no target, or any target outside the workspace, fails
/// closed before a grant or the mode is looked at.
fn all_inside(targets: &[ClassifiedTarget]) -> bool {
    !targets.is_empty() && targets.iter().all(|t| t.target == Target::Inside)
}

fn grant_or_prompt(granted: bool) -> Verdict {
    if granted {
        Verdict::Allow
    } else {
        Verdict::Prompt
    }
}

fn decide_edit(input: &PolicyInput<'_>) -> Verdict {
    if !all_inside(input.targets) {
        return Verdict::Deny;
    }
    if input.mode == PermissionMode::AcceptEdits {
        // A grant never overrides the prompt for a path that changes what runs.
        let affecting = input
            .targets
            .iter()
            .any(|t| target_affects_execution(input.workspace_root, &t.path));
        return if affecting {
            Verdict::Prompt
        } else {
            Verdict::Allow
        };
    }
    grant_or_prompt(input.granted)
}

/// Whether editing `path` can change what a build or hook executes, by the name
/// it is given or by the place it really is once symlinks are followed. A link
/// into `.git` or a guarded directory has a harmless name, so the name alone
/// would let it through.
fn target_affects_execution(workspace_root: &Path, path: &Path) -> bool {
    if is_execution_affecting(workspace_root, path) {
        return true;
    }
    resolve_in_workspace(workspace_root, path)
        .is_none_or(|real| is_execution_affecting(workspace_root, &real))
}

/// Whether editing `path` can change what a build or hook executes.
///
/// Fails closed: an absolute path that does not sit under `workspace_root` as
/// spelled (for example a canonical `/private/tmp/ws` against `/tmp/ws`), or a
/// `..` that climbs above its start, cannot be matched against root-anchored
/// directories and so counts as execution affecting.
#[must_use]
pub fn is_execution_affecting(workspace_root: &Path, path: &Path) -> bool {
    let relative = if path.is_absolute() {
        match path.strip_prefix(workspace_root) {
            Ok(relative) => relative,
            Err(_) => return true,
        }
    } else {
        path
    };
    let Some(parts) = lexical_components(relative) else {
        return true;
    };
    let Some(file_name) = parts.last() else {
        return false;
    };
    if parts.iter().any(|part| part.eq_ignore_ascii_case(GIT_DIR)) {
        return true;
    }
    if EXECUTION_FILE_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(file_name))
    {
        return true;
    }
    if parts.ends_with(&[CARGO_DIR.to_owned(), CARGO_CONFIG_FILE.to_owned()]) {
        return true;
    }
    parts.len() > 1
        && ROOT_EXECUTION_DIRS
            .iter()
            .any(|dir| parts.starts_with(&dir.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>()))
}

/// Normal components with `.` dropped and `..` applied to the previous one;
/// `None` when a `..` climbs above the start.
fn lexical_components(path: &Path) -> Option<Vec<String>> {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => parts.push(name.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => {}
        }
    }
    Some(parts)
}

/// A stored grant: one exact tool, command and place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredGrant {
    /// Canonical git root the grant was given in.
    pub repo: PathBuf,
    /// Stable tool name, see [`Tool::name`].
    pub tool: String,
    /// Working directory the grant was given in.
    pub cwd: PathBuf,
    /// What the grant covers, see [`grant_pattern`].
    pub pattern: String,
}

/// Separates target paths in an edit grant pattern; no path component holds it.
const PATH_SEPARATOR: char = '\u{1f}';

/// The exact argument a grant covers: the normalized, sorted target paths for
/// an edit, the command line for other tools, empty when there is none.
#[must_use]
pub fn grant_pattern(tool: &Tool, command: Option<&str>, target_paths: &[PathBuf]) -> String {
    if !matches!(tool, Tool::Edit | Tool::Write | Tool::MultiEdit) {
        return command.unwrap_or_default().to_owned();
    }
    let mut paths: Vec<String> = target_paths.iter().map(|p| normalized(p)).collect();
    paths.sort();
    paths.dedup();
    paths.join(&PATH_SEPARATOR.to_string())
}

/// `path` with `.` dropped and `..` applied; spelled as given when a `..`
/// climbs above its start.
fn normalized(path: &Path) -> String {
    let Some(parts) = lexical_components(path) else {
        return path.to_string_lossy().into_owned();
    };
    let root = if path.is_absolute() { "/" } else { "" };
    format!("{root}{}", parts.join("/"))
}

/// Whether `stored` covers this request. Every field must match exactly:
/// a grant never extends to another repo, directory or argument.
#[must_use]
pub fn grant_matches(
    stored: &StoredGrant,
    repo: &Path,
    tool: &Tool,
    cwd: &Path,
    pattern: &str,
) -> bool {
    stored.repo == repo
        && stored.tool == tool.name()
        && stored.cwd == cwd
        && stored.pattern == pattern
}

/// One row of the decision table.
///
/// The WU-18 session gate and the permission broker run the same rows.
#[derive(Debug, Clone)]
pub struct TableRow {
    /// Short description, shown when a row fails.
    pub name: &'static str,
    /// Answering policy.
    pub mode: PermissionMode,
    /// Requested tool.
    pub tool: Tool,
    /// Classified target paths.
    pub targets: Vec<ClassifiedTarget>,
    /// Repo identity of the request.
    pub repo: PathBuf,
    /// Working directory of the request.
    pub cwd: PathBuf,
    /// Command line of the request, for shell tools.
    pub command: Option<String>,
    /// The grant in the store, if any.
    pub stored: Option<StoredGrant>,
    /// The expected verdict.
    pub expected: Verdict,
    /// True while the tool is not implemented yet; consumers that lack the
    /// tool skip the row.
    pub pending: bool,
}

impl TableRow {
    /// The grant pattern of this row's request.
    fn pattern(&self) -> String {
        let paths: Vec<PathBuf> = self.targets.iter().map(|t| t.path.clone()).collect();
        grant_pattern(&self.tool, self.command.as_deref(), &paths)
    }

    /// Whether the stored grant covers this row's request.
    #[must_use]
    pub fn granted(&self) -> bool {
        self.stored.as_ref().is_some_and(|stored| {
            grant_matches(stored, &self.repo, &self.tool, &self.cwd, &self.pattern())
        })
    }
}

/// Repo and working directory the table's requests run in by default.
const TABLE_REPO: &str = "/ws";
const TABLE_CWD: &str = "/ws";

/// Workspace root the table's relative paths are named against.
pub const TABLE_WORKSPACE_ROOT: &str = "/ws";

/// The decision table as data.
#[must_use]
pub fn decision_table() -> Vec<TableRow> {
    let mut rows = Rows(Vec::new());
    deny_all_rows(&mut rows);
    for mode in [PermissionMode::Prompt, PermissionMode::AcceptEdits] {
        path_tool_rows(&mut rows, mode);
        edit_escape_rows(&mut rows, mode);
        bash_and_scope_rows(&mut rows, mode);
    }
    edit_prompt_and_allow_rows(&mut rows);
    write_rows(&mut rows);
    rows.0
}

/// Collects rows, deriving `pending` from the tool.
struct Rows(Vec<TableRow>);

impl Rows {
    fn add(
        &mut self,
        name: &'static str,
        mode: PermissionMode,
        tool: Tool,
        targets: Vec<ClassifiedTarget>,
        granted: bool,
        expected: Verdict,
    ) {
        let grant = granted.then(|| targets.clone());
        self.add_granting(name, mode, tool, targets, grant, expected);
    }

    /// Like [`Rows::add`], with a grant for exactly the `granted` targets.
    fn add_granting(
        &mut self,
        name: &'static str,
        mode: PermissionMode,
        tool: Tool,
        targets: Vec<ClassifiedTarget>,
        granted: Option<Vec<ClassifiedTarget>>,
        expected: Verdict,
    ) {
        let stored = granted.map(|granted| {
            let paths: Vec<PathBuf> = granted.into_iter().map(|t| t.path).collect();
            StoredGrant {
                repo: PathBuf::from(TABLE_REPO),
                tool: tool.name().to_owned(),
                cwd: PathBuf::from(TABLE_CWD),
                pattern: grant_pattern(&tool, None, &paths),
            }
        });
        self.push(
            name,
            mode,
            tool,
            targets,
            Scope::default(),
            stored,
            expected,
        );
    }

    fn add_bash(&mut self, row: BashRow) {
        let stored = row.stored.map(|(repo, cwd, pattern)| StoredGrant {
            repo: PathBuf::from(repo),
            tool: Tool::Bash.name().to_owned(),
            cwd: PathBuf::from(cwd),
            pattern: pattern.to_owned(),
        });
        let scope = Scope {
            repo: row.repo,
            cwd: row.cwd,
            command: Some(row.command),
        };
        self.push(
            row.name,
            row.mode,
            Tool::Bash,
            Vec::new(),
            scope,
            stored,
            row.expected,
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per column of the policy table"
    )]
    fn push(
        &mut self,
        name: &'static str,
        mode: PermissionMode,
        tool: Tool,
        targets: Vec<ClassifiedTarget>,
        scope: Scope,
        stored: Option<StoredGrant>,
        expected: Verdict,
    ) {
        let pending = matches!(
            tool,
            Tool::CodeReadRange
                | Tool::Retrieve
                | Tool::CodeSearch
                | Tool::CodeSymbol
                | Tool::CodeOutline
        );
        self.0.push(TableRow {
            name,
            mode,
            tool,
            targets,
            repo: PathBuf::from(scope.repo),
            cwd: PathBuf::from(scope.cwd),
            command: scope.command.map(str::to_owned),
            stored,
            expected,
            pending,
        });
    }
}

/// Where a request runs.
#[derive(Clone, Copy)]
struct Scope {
    repo: &'static str,
    cwd: &'static str,
    command: Option<&'static str>,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            repo: TABLE_REPO,
            cwd: TABLE_CWD,
            command: None,
        }
    }
}

/// A bash row: the request, and a stored grant as (repo, cwd, pattern).
#[derive(Clone, Copy)]
struct BashRow {
    name: &'static str,
    mode: PermissionMode,
    repo: &'static str,
    cwd: &'static str,
    command: &'static str,
    stored: Option<(&'static str, &'static str, &'static str)>,
    expected: Verdict,
}

fn inside(path: &str) -> Vec<ClassifiedTarget> {
    vec![target(path, Target::Inside)]
}

fn outside() -> Vec<ClassifiedTarget> {
    vec![target("/etc/passwd", Target::Outside)]
}

fn escapes() -> Vec<ClassifiedTarget> {
    vec![target("link/secret", Target::Escapes)]
}

fn unknown(name: &str) -> Tool {
    Tool::Unknown(name.to_owned())
}

fn deny_all_rows(rows: &mut Rows) {
    let tools = [
        Tool::Read,
        Tool::Edit,
        Tool::Bash,
        Tool::Retrieve,
        unknown("Task"),
    ];
    for tool in tools {
        let mode = PermissionMode::DenyAll;
        rows.add(
            "deny-all refuses everything",
            mode,
            tool.clone(),
            inside("a.rs"),
            false,
            Verdict::Deny,
        );
        rows.add(
            "deny-all beats a grant",
            mode,
            tool,
            inside("a.rs"),
            true,
            Verdict::Deny,
        );
    }
}

fn path_tool_rows(rows: &mut Rows, mode: PermissionMode) {
    for tool in [Tool::Read, Tool::Grep, Tool::List, Tool::CodeReadRange] {
        rows.add(
            "read inside the workspace",
            mode,
            tool.clone(),
            inside("src/a.rs"),
            false,
            Verdict::Allow,
        );
        rows.add(
            "read outside the workspace",
            mode,
            tool.clone(),
            outside(),
            false,
            Verdict::Deny,
        );
        rows.add(
            "read through a symlink escape",
            mode,
            tool.clone(),
            escapes(),
            false,
            Verdict::Deny,
        );
        rows.add(
            "read with no target",
            mode,
            tool,
            Vec::new(),
            false,
            Verdict::Deny,
        );
    }
}

fn edit_escape_rows(rows: &mut Rows, mode: PermissionMode) {
    let mixed = vec![
        target("a.rs", Target::Inside),
        target("/tmp/b", Target::Outside),
    ];
    rows.add(
        "edit outside the workspace",
        mode,
        Tool::Edit,
        outside(),
        false,
        Verdict::Deny,
    );
    rows.add(
        "edit through a symlink escape",
        mode,
        Tool::Edit,
        escapes(),
        false,
        Verdict::Deny,
    );
    rows.add(
        "edit outside beats a grant",
        mode,
        Tool::Edit,
        outside(),
        true,
        Verdict::Deny,
    );
    rows.add(
        "edit with one target outside",
        mode,
        Tool::Edit,
        mixed,
        false,
        Verdict::Deny,
    );
    rows.add(
        "edit with no target",
        mode,
        Tool::Edit,
        Vec::new(),
        false,
        Verdict::Deny,
    );
}

fn bash_and_scope_rows(rows: &mut Rows, mode: PermissionMode) {
    bash_grant_rows(rows, mode);
    for tool in [
        Tool::Retrieve,
        Tool::CodeSearch,
        Tool::CodeSymbol,
        Tool::CodeOutline,
        Tool::Skill,
        Tool::GitStatus,
        Tool::GitDiff,
        Tool::Recall,
    ] {
        rows.add(
            "session scope tool",
            mode,
            tool,
            Vec::new(),
            false,
            Verdict::Allow,
        );
    }
    rows.add(
        "fetch with no grant",
        mode,
        Tool::Fetch,
        Vec::new(),
        false,
        Verdict::Prompt,
    );
    rows.add(
        "fetch with a grant for its address",
        mode,
        Tool::Fetch,
        Vec::new(),
        true,
        Verdict::Allow,
    );
    rows.add(
        "unknown tool WebFetch",
        mode,
        unknown("WebFetch"),
        Vec::new(),
        false,
        Verdict::Prompt,
    );
    rows.add(
        "unknown tool Task",
        mode,
        unknown("Task"),
        Vec::new(),
        false,
        Verdict::Prompt,
    );
}

fn bash_grant_rows(rows: &mut Rows, mode: PermissionMode) {
    let grant = Some(("/ws", "/ws/x", "cargo test"));
    let cases: [(&str, &str, &str, &str, Option<_>, Verdict); 9] = [
        (
            "bash with no grant",
            "/ws",
            "/ws/x",
            "cargo test",
            None,
            Verdict::Prompt,
        ),
        (
            "bash granted for this repo, cwd and command",
            "/ws",
            "/ws/x",
            "cargo test",
            grant,
            Verdict::Allow,
        ),
        (
            "bash granted in another repo only",
            "/other",
            "/ws/x",
            "cargo test",
            grant,
            Verdict::Prompt,
        ),
        (
            "bash granted at another cwd only",
            "/ws",
            "/ws/y",
            "cargo test",
            grant,
            Verdict::Prompt,
        ),
        (
            "bash chained with a semicolon",
            "/ws",
            "/ws/x",
            "cargo test; rm -rf x",
            grant,
            Verdict::Prompt,
        ),
        (
            "bash chained with and",
            "/ws",
            "/ws/x",
            "cargo test && x",
            grant,
            Verdict::Prompt,
        ),
        (
            "bash with a command substitution",
            "/ws",
            "/ws/x",
            "cargo test $(x)",
            grant,
            Verdict::Prompt,
        ),
        (
            "bash with extra whitespace",
            "/ws",
            "/ws/x",
            "cargo  test",
            grant,
            Verdict::Prompt,
        ),
        (
            "bash with a different command",
            "/ws",
            "/ws/x",
            "cargo build",
            grant,
            Verdict::Prompt,
        ),
    ];
    for (name, repo, cwd, command, stored, expected) in cases {
        rows.add_bash(BashRow {
            name,
            mode,
            repo,
            cwd,
            command,
            stored,
            expected,
        });
    }
}

fn edit_prompt_and_allow_rows(rows: &mut Rows) {
    use PermissionMode::{AcceptEdits, Prompt};
    rows.add(
        "ask prompts for an inside edit",
        Prompt,
        Tool::Edit,
        inside("src/a.rs"),
        false,
        Verdict::Prompt,
    );
    rows.add(
        "ask allows a granted inside edit",
        Prompt,
        Tool::Edit,
        inside("src/a.rs"),
        true,
        Verdict::Allow,
    );
    rows.add(
        "accept-edits allows a plain edit",
        AcceptEdits,
        Tool::Edit,
        inside("src/a.rs"),
        false,
        Verdict::Allow,
    );
    rows.add_granting(
        "a grant for one file does not allow another",
        Prompt,
        Tool::Edit,
        inside("src/b.rs"),
        Some(inside("src/a.rs")),
        Verdict::Prompt,
    );
    rows.add_granting(
        "a grant for one file does not allow Cargo.toml",
        Prompt,
        Tool::Edit,
        inside("Cargo.toml"),
        Some(inside("src/a.rs")),
        Verdict::Prompt,
    );
    rows.add_granting(
        "ask allows an exactly granted execution-affecting edit",
        Prompt,
        Tool::Edit,
        inside("Cargo.toml"),
        Some(inside("Cargo.toml")),
        Verdict::Allow,
    );
    rows.add_granting(
        "a grant does not override accept-edits on an execution-affecting edit",
        AcceptEdits,
        Tool::Edit,
        inside("Cargo.toml"),
        Some(inside("Cargo.toml")),
        Verdict::Prompt,
    );
    let affecting = [
        "Cargo.toml",
        "crates/x/build.rs",
        "rust-toolchain.toml",
        ".cargo/config.toml",
        ".git/hooks/pre-commit",
        ".github/workflows/ci.yml",
        "scripts/run.sh",
    ];
    for path in affecting {
        rows.add(
            "accept-edits prompts for an execution-affecting edit",
            AcceptEdits,
            Tool::Edit,
            inside(path),
            false,
            Verdict::Prompt,
        );
    }
}

/// `write` is decided exactly like `edit`: it changes files, so it prompts
/// unless accept-edits or a grant allows it, and never leaves the workspace.
fn write_rows(rows: &mut Rows) {
    use PermissionMode::{AcceptEdits, Prompt};
    multi_edit_rows(rows);
    rows.add(
        "ask prompts for an inside write",
        Prompt,
        Tool::Write,
        inside("src/new.rs"),
        false,
        Verdict::Prompt,
    );
    rows.add(
        "ask allows a granted inside write",
        Prompt,
        Tool::Write,
        inside("src/new.rs"),
        true,
        Verdict::Allow,
    );
    rows.add(
        "accept-edits allows a plain write",
        AcceptEdits,
        Tool::Write,
        inside("src/new.rs"),
        false,
        Verdict::Allow,
    );
    rows.add(
        "accept-edits prompts for an execution-affecting write",
        AcceptEdits,
        Tool::Write,
        inside("build.rs"),
        false,
        Verdict::Prompt,
    );
    for mode in [Prompt, AcceptEdits] {
        rows.add(
            "write outside the workspace",
            mode,
            Tool::Write,
            outside(),
            true,
            Verdict::Deny,
        );
        rows.add(
            "write through a symlink escape",
            mode,
            Tool::Write,
            escapes(),
            false,
            Verdict::Deny,
        );
        rows.add(
            "write with no target",
            mode,
            Tool::Write,
            Vec::new(),
            false,
            Verdict::Deny,
        );
    }
}

/// `multi_edit` is one request for every file it names, decided like `edit`:
/// one path outside the workspace denies the whole call, and one
/// execution-affecting path makes accept-edits ask.
fn multi_edit_rows(rows: &mut Rows) {
    use PermissionMode::{AcceptEdits, Prompt};
    let two = || {
        vec![
            target("src/a.rs", Target::Inside),
            target("src/b.rs", Target::Inside),
        ]
    };
    rows.add(
        "ask prompts for a multi-file edit",
        Prompt,
        Tool::MultiEdit,
        two(),
        false,
        Verdict::Prompt,
    );
    rows.add(
        "accept-edits allows a multi-file edit",
        AcceptEdits,
        Tool::MultiEdit,
        two(),
        false,
        Verdict::Allow,
    );
    rows.add_granting(
        "a grant for one file does not allow a multi-file edit",
        Prompt,
        Tool::MultiEdit,
        two(),
        Some(inside("src/a.rs")),
        Verdict::Prompt,
    );
    rows.add(
        "accept-edits prompts when one of the files is execution-affecting",
        AcceptEdits,
        Tool::MultiEdit,
        vec![
            target("src/a.rs", Target::Inside),
            target("build.rs", Target::Inside),
        ],
        false,
        Verdict::Prompt,
    );
    for mode in [Prompt, AcceptEdits] {
        rows.add(
            "a multi-file edit with one target outside",
            mode,
            Tool::MultiEdit,
            vec![
                target("src/a.rs", Target::Inside),
                target("/tmp/b", Target::Outside),
            ],
            true,
            Verdict::Deny,
        );
        rows.add(
            "a multi-file edit with no target",
            mode,
            Tool::MultiEdit,
            Vec::new(),
            false,
            Verdict::Deny,
        );
    }
}

fn target(path: &str, target: Target) -> ClassifiedTarget {
    ClassifiedTarget {
        path: PathBuf::from(path),
        target,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coda_protocol::paths::classify_target;

    const ROOT: &str = "/ws";

    #[test]
    fn execution_affecting_paths_are_recognised() {
        // Arrange
        let affecting = [
            "Cargo.toml",
            "crates/x/Cargo.toml",
            "crates/x/build.rs",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            ".git/hooks/pre-commit",
            ".github/workflows/ci.yml",
            "scripts/run.sh",
            "/ws/scripts/run.sh",
            "/private/tmp/ws/scripts/run.sh",
            "/elsewhere/src/lib.rs",
            "/ws/../ws/scripts/run.sh",
            "../ws/scripts/run.sh",
            "src/../Cargo.toml",
            "./Cargo.toml",
        ];
        let plain = [
            "src/main.rs",
            "README.md",
            "crates/x/scripts/a.sh",
            "docs/Cargo.toml.md",
            "/ws/src/lib.rs",
        ];

        for path in affecting {
            // Act
            let result = is_execution_affecting(Path::new(ROOT), Path::new(path));

            // Assert
            assert!(result, "{path} should be execution affecting");
        }
        for path in plain {
            // Act
            let result = is_execution_affecting(Path::new(ROOT), Path::new(path));

            // Assert
            assert!(!result, "{path} should be plain");
        }
    }

    #[test]
    fn accept_edits_prompts_for_every_build_and_package_file_with_each_file_tool() {
        // Arrange
        let names = [
            "Makefile",
            "makefile",
            "GNUmakefile",
            "justfile",
            "Justfile",
            "MAKEFILE",
            "Package.json",
            "dockerfile",
            "cargo.toml",
            "package.json",
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "tox.ini",
            "noxfile.py",
            "conftest.py",
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
            "pom.xml",
            "CMakeLists.txt",
            "Rakefile",
            "Gemfile",
            "Dockerfile",
            "Taskfile.yml",
            "Taskfile.yaml",
            ".envrc",
            ".npmrc",
            ".pre-commit-config.yaml",
        ];

        for name in names {
            for tool in [Tool::Edit, Tool::Write, Tool::MultiEdit] {
                for path in [name.to_owned(), format!("packages/app/{name}")] {
                    let targets = [ClassifiedTarget {
                        path: PathBuf::from(&path),
                        target: Target::Inside,
                    }];
                    let input = PolicyInput {
                        mode: PermissionMode::AcceptEdits,
                        tool: &tool,
                        workspace_root: Path::new(ROOT),
                        targets: &targets,
                        granted: true,
                    };

                    // Act
                    let verdict = decide(&input);

                    // Assert
                    assert_eq!(verdict, Verdict::Prompt, "{tool:?} on {path}");
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn accept_edits_prompts_for_a_symlink_that_points_at_a_guarded_path() {
        use std::os::unix::fs::symlink;

        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for guarded in [".git", "scripts", ".github/workflows"] {
            std::fs::create_dir_all(root.join(guarded)).unwrap();
        }
        std::fs::write(root.join(".git/config"), "").unwrap();
        std::fs::write(root.join("scripts/build.sh"), "").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        symlink(".git", root.join("to_git")).unwrap();
        symlink("scripts", root.join("to_scripts")).unwrap();
        symlink(".github/workflows", root.join("to_workflows")).unwrap();
        symlink("../.git/config", root.join("src/to_config")).unwrap();
        symlink("scripts/build.sh", root.join("to_build_file")).unwrap();
        let paths = [
            "to_git",
            "to_git/config",
            "to_scripts/new.sh",
            "to_scripts/build.sh",
            "to_workflows/ci.yml",
            "src/to_config",
            "to_build_file",
        ];

        for path in paths {
            for tool in [Tool::Edit, Tool::Write, Tool::MultiEdit] {
                let targets = [ClassifiedTarget {
                    path: PathBuf::from(path),
                    target: classify_target(&root, Path::new(path)),
                }];
                let input = PolicyInput {
                    mode: PermissionMode::AcceptEdits,
                    tool: &tool,
                    workspace_root: &root,
                    targets: &targets,
                    granted: true,
                };

                // Act
                let verdict = decide(&input);

                // Assert
                assert_eq!(verdict, Verdict::Prompt, "{tool:?} on {path}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn accept_edits_allows_a_symlink_that_points_at_a_plain_file() {
        use std::os::unix::fs::symlink;

        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("notes.txt"), "").unwrap();
        symlink("notes.txt", root.join("alias.txt")).unwrap();
        let targets = [ClassifiedTarget {
            path: PathBuf::from("alias.txt"),
            target: classify_target(&root, Path::new("alias.txt")),
        }];
        let input = PolicyInput {
            mode: PermissionMode::AcceptEdits,
            tool: &Tool::Write,
            workspace_root: &root,
            targets: &targets,
            granted: false,
        };

        // Act
        let verdict = decide(&input);

        // Assert
        assert_eq!(verdict, Verdict::Allow);
    }

    #[test]
    fn accept_edits_prompts_for_git_metadata_at_the_root_and_nested() {
        // Arrange
        let paths = [
            ".git/config",
            ".git/info/exclude",
            ".git/info/attributes",
            ".git/modules/sub/config",
            ".git",
            ".GIT/config",
            ".gitattributes",
            ".GitAttributes",
            "packages/app/.git/config",
            "packages/app/.git/info/attributes",
            "packages/app/.git",
            "./.git/config",
            "src/../.git/config",
            ".git/config/",
            "packages/app/.gitattributes",
        ];

        for path in paths {
            for tool in [Tool::Edit, Tool::Write, Tool::MultiEdit] {
                let targets = [ClassifiedTarget {
                    path: PathBuf::from(path),
                    target: Target::Inside,
                }];
                let input = PolicyInput {
                    mode: PermissionMode::AcceptEdits,
                    tool: &tool,
                    workspace_root: Path::new(ROOT),
                    targets: &targets,
                    granted: false,
                };

                // Act
                let verdict = decide(&input);

                // Assert
                assert_eq!(verdict, Verdict::Prompt, "{tool:?} on {path}");
            }
        }
    }

    #[test]
    fn files_that_merely_resemble_git_metadata_stay_plain() {
        // Arrange
        let plain = [
            ".gitignore",
            "docs/.github/readme.md",
            "src/git/config",
            "a.git/config",
        ];

        for path in plain {
            // Act
            let result = is_execution_affecting(Path::new(ROOT), Path::new(path));

            // Assert
            assert!(!result, "{path} should be plain");
        }
    }

    #[test]
    fn accept_edits_prompts_when_an_inside_path_does_not_share_the_root_spelling() {
        // Arrange
        let targets = [ClassifiedTarget {
            path: PathBuf::from("/private/tmp/ws/.git/hooks/pre-commit"),
            target: Target::Inside,
        }];
        let input = PolicyInput {
            mode: PermissionMode::AcceptEdits,
            tool: &Tool::Edit,
            workspace_root: Path::new("/tmp/ws"),
            targets: &targets,
            granted: false,
        };

        // Act
        let verdict = decide(&input);

        // Assert
        assert_eq!(verdict, Verdict::Prompt);
    }

    #[test]
    fn a_grant_matches_only_the_exact_repo_tool_cwd_and_pattern() {
        // Arrange
        let stored = StoredGrant {
            repo: PathBuf::from("/a"),
            tool: "bash".to_owned(),
            cwd: PathBuf::from("/a/x"),
            pattern: "cargo test".to_owned(),
        };
        let cases = [
            ("/a", Tool::Bash, "/a/x", Some("cargo test"), true),
            ("/b", Tool::Bash, "/a/x", Some("cargo test"), false),
            ("/a", Tool::Bash, "/a/y", Some("cargo test"), false),
            ("/a", Tool::Edit, "/a/x", Some("cargo test"), false),
            ("/a", Tool::Bash, "/a/x", Some("cargo  test"), false),
            (
                "/a",
                Tool::Bash,
                "/a/x",
                Some("cargo test; rm -rf x"),
                false,
            ),
            ("/a", Tool::Bash, "/a/x", None, false),
            // Codex reports shell commands wrapped like this; the wrapped form
            // is a different command, so a plain grant never covers it.
            (
                "/a",
                Tool::Bash,
                "/a/x",
                Some("/bin/zsh -lc 'cargo test'"),
                false,
            ),
        ];

        for (repo, tool, cwd, command, expected) in cases {
            // Act
            let pattern = grant_pattern(&tool, command, &[]);
            let matched = grant_matches(&stored, Path::new(repo), &tool, Path::new(cwd), &pattern);

            // Assert
            assert_eq!(matched, expected, "{repo} {tool:?} {cwd} {command:?}");
        }
    }

    #[test]
    fn an_edit_pattern_is_the_sorted_normalized_target_paths() {
        // Arrange
        let paths = [
            PathBuf::from("src/./b.rs"),
            PathBuf::from("src/x/../a.rs"),
            PathBuf::from("src/a.rs"),
        ];

        // Act
        let pattern = grant_pattern(&Tool::Edit, Some("ignored"), &paths);

        // Assert
        assert_eq!(pattern, "src/a.rs\u{1f}src/b.rs");
    }

    #[test]
    fn decision_table_rows_decide_as_recorded() {
        // Arrange
        let rows = decision_table();

        for row in rows {
            let input = PolicyInput {
                mode: row.mode,
                tool: &row.tool,
                workspace_root: Path::new(TABLE_WORKSPACE_ROOT),
                targets: &row.targets,
                granted: row.granted(),
            };

            // Act
            let verdict = decide(&input);

            // Assert
            assert_eq!(verdict, row.expected, "row: {} ({:?})", row.name, row.mode);
        }
    }

    #[test]
    fn decision_table_covers_every_mode_and_verdict() {
        // Arrange
        let rows = decision_table();

        // Act
        let has = |mode, verdict| rows.iter().any(|r| r.mode == mode && r.expected == verdict);

        // Assert
        for mode in [PermissionMode::Prompt, PermissionMode::AcceptEdits] {
            for verdict in [Verdict::Allow, Verdict::Deny, Verdict::Prompt] {
                assert!(has(mode, verdict), "{mode:?} lacks a {verdict:?} row");
            }
        }
        assert!(has(PermissionMode::DenyAll, Verdict::Deny));
    }

    #[test]
    fn pending_rows_are_exactly_those_for_unimplemented_tools() {
        // Arrange
        let rows = decision_table();

        for row in rows {
            // Act
            let unimplemented = matches!(
                row.tool,
                Tool::CodeReadRange
                    | Tool::Retrieve
                    | Tool::CodeSearch
                    | Tool::CodeSymbol
                    | Tool::CodeOutline
            );

            // Assert
            assert_eq!(row.pending, unimplemented, "row: {}", row.name);
        }
    }
}
