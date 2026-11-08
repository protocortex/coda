// SPDX-License-Identifier: Apache-2.0

//! `session.ship`: commit what a session changed, push the branch and open a
//! pull request, each through the user's own `git` and `gh`.
//!
//! Signing, sign-off and the `gh` login come from the user's git and `gh`
//! setup. Nothing here reads a token or a credential file, and nothing forces a
//! push. The text of the commit and of the pull request comes from the request.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use coda_protocol::methods::{SessionShipParams, SessionShipResult, ShipAction};
use coda_selfhost::process::run_bounded;

use crate::worktree::INHERITED_GIT_VARIABLES;

const GIT_LIMIT: Duration = Duration::from_secs(120);
/// A push may run hooks and upload a lot, and `gh` calls the network.
const NETWORK_LIMIT: Duration = Duration::from_secs(300);
const REMOTE: &str = "origin";
/// Longest message or body the daemon accepts.
pub(crate) const MAX_SHIP_TEXT_BYTES: usize = 64 * 1024;
/// Stops git and `gh` from waiting on a prompt nobody can answer.
const NO_PROMPT_VARIABLES: [(&str, &str); 2] =
    [("GIT_TERMINAL_PROMPT", "0"), ("GH_PROMPT_DISABLED", "1")];
const URL_PREFIX: &str = "https://";
const PULL_PATH: &str = "/pull/";
const PULL_WORD: &str = "pull";
const PR_CREATE_WORDS: [&str; 3] = ["gh", "pr", "create"];
/// Characters that start shell syntax the word splitter does not read: escapes,
/// command substitution, subshells, groups, heredocs and comments. They count even inside
/// quotes, since a line that holds one is not worth the risk of misreading.
const UNREAD_SHELL_SYNTAX: [char; 8] = ['\\', '`', '(', ')', '{', '}', '<', '#'];
/// A pull request address is `https://host/owner/repo/pull/number`: this many path parts.
const PULL_PATH_PARTS: usize = 4;
const PULL_WORD_INDEX: usize = 2;
/// What the shell tool appends to the output of a command that exited with a failure.
const EXIT_STATUS_MARKER: &str = "[exit status";
/// Longest pull request address kept.
const MAX_URL_BYTES: usize = 512;
/// Branch names that are never shipped to, whatever `origin/HEAD` says.
const PROTECTED_BRANCHES: [&str; 2] = ["main", "master"];

/// The programs a ship runs, so a test can stand in for `gh`.
#[derive(Debug, Clone)]
pub(crate) struct ShipTools {
    git: OsString,
    gh: OsString,
}

impl Default for ShipTools {
    fn default() -> Self {
        Self {
            git: OsString::from("git"),
            gh: OsString::from("gh"),
        }
    }
}

impl ShipTools {
    #[cfg(test)]
    pub(crate) fn with_gh(gh: OsString) -> Self {
        Self {
            gh,
            ..Self::default()
        }
    }
}

/// Runs a ship in `cwd`. An error holds the reason, in words, for the caller;
/// steps that ran before it are not undone.
pub(crate) fn ship(
    tools: &ShipTools,
    cwd: &Path,
    params: &SessionShipParams,
) -> Result<SessionShipResult, String> {
    let message = params.message.trim();
    if message.is_empty() {
        return Err("the commit message is empty".to_owned());
    }
    let too_long = |text: &str| text.len() > MAX_SHIP_TEXT_BYTES;
    if too_long(message) || params.body.as_deref().is_some_and(too_long) {
        return Err(format!(
            "the message and body must each be at most {MAX_SHIP_TEXT_BYTES} bytes"
        ));
    }
    let branch = run(
        &tools.git,
        cwd,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        GIT_LIMIT,
    )
    .map_err(|_| "HEAD is not on a branch".to_owned())?
    .trim()
    .to_owned();
    if params.action != ShipAction::Commit && is_default_branch(tools, cwd, &branch) {
        return Err(format!(
            "{branch} is the default branch; make a branch for the change before pushing"
        ));
    }
    if params.all {
        run(&tools.git, cwd, &["add", "--all"], GIT_LIMIT)?;
    }
    let staged = run(
        &tools.git,
        cwd,
        &["diff", "--cached", "--name-only", "--no-renames", "-z"],
        GIT_LIMIT,
    )?;
    let files: Vec<String> = staged
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    if files.is_empty() {
        return Err(if params.all {
            "there is nothing to commit".to_owned()
        } else {
            "there is nothing to commit: stage files first, or ship with --all to stage every change"
                .to_owned()
        });
    }
    run(
        &tools.git,
        cwd,
        &["commit", "--message", message],
        GIT_LIMIT,
    )?;
    let commit = run(&tools.git, cwd, &["rev-parse", "HEAD"], GIT_LIMIT)?
        .trim()
        .to_owned();
    let mut result = SessionShipResult {
        commit,
        branch: branch.clone(),
        files,
        pushed: false,
        pull_request: None,
    };
    if params.action == ShipAction::Commit {
        return Ok(result);
    }
    run(
        &tools.git,
        cwd,
        &["push", "--set-upstream", REMOTE, &branch],
        NETWORK_LIMIT,
    )?;
    result.pushed = true;
    if params.action == ShipAction::CommitPush {
        return Ok(result);
    }
    result.pull_request = Some(open_pull_request(tools, cwd, params, message)?);
    Ok(result)
}

fn is_default_branch(tools: &ShipTools, cwd: &Path, branch: &str) -> bool {
    if PROTECTED_BRANCHES.contains(&branch) {
        return true;
    }
    run(
        &tools.git,
        cwd,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        GIT_LIMIT,
    )
    .is_ok_and(|head| head.trim().strip_prefix("origin/") == Some(branch))
}

fn open_pull_request(
    tools: &ShipTools,
    cwd: &Path,
    params: &SessionShipParams,
    message: &str,
) -> Result<String, String> {
    let title = message.lines().next().unwrap_or(message);
    let body = params.body.as_deref().unwrap_or_default();
    let mut args = vec!["pr", "create", "--title", title, "--body", body];
    if !params.ready {
        args.push("--draft");
    }
    let printed = run(&tools.gh, cwd, &args, NETWORK_LIMIT)?;
    printed_pull_request(&printed)
        .ok_or_else(|| "gh did not print the address of the pull request".to_owned())
}

/// Whether some command of the shell line `command` is `gh pr create`, so its output
/// holds a pull request address. Text inside quotes or after another command's name
/// does not count. A line with escapes, subshells, groups or redirections is not read at
/// all, so it never counts: a lost link is better than a planted one.
pub(crate) fn creates_pull_request(command: &str) -> bool {
    if command.contains(UNREAD_SHELL_SYNTAX) {
        return false;
    }
    command_words(command).iter().any(|words| {
        words
            .iter()
            .map(String::as_str)
            .skip_while(|w| is_assignment(w))
            .take(PR_CREATE_WORDS.len())
            .eq(PR_CREATE_WORDS)
    })
}

/// A word like `NAME=value`, which sets a variable for the command after it.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// The words of each command of a shell line, split at unquoted `;`, `&`, `|` and
/// newlines, with quotes removed. It reads only what is needed to find a command's name.
fn command_words(line: &str) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, ';' | '&' | '|' | '\n') => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                if !words.is_empty() {
                    commands.push(std::mem::take(&mut words));
                }
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    if !words.is_empty() {
        commands.push(words);
    }
    commands
}

/// The host `origin` points at. Compared without case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OriginHost(String);

impl OriginHost {
    /// Reads the host of a remote in the forms `https://host/owner/repo(.git)`,
    /// `ssh://user@host(:port)/owner/repo(.git)` and `user@host:owner/repo(.git)`. A path on
    /// disk, a `file:` address or an address without an owner and a repository gives `None`.
    pub(crate) fn parse(remote: &str) -> Option<Self> {
        let remote = remote.trim();
        let (authority, path, port_is_part_of_host) = match remote.split_once("://") {
            Some((scheme @ ("https" | "http" | "ssh" | "git"), rest)) => {
                let (authority, path) = rest.split_once('/')?;
                (authority, path, matches!(scheme, "https" | "http"))
            }
            Some(_) => return None,
            None => {
                let (authority, path) = remote.split_once(':')?;
                (authority, path, false)
            }
        };
        let host = authority.rsplit('@').next().unwrap_or(authority);
        let host = if port_is_part_of_host {
            host
        } else {
            host.split(':').next().unwrap_or(host)
        };
        let mut parts = path
            .trim_matches('/')
            .trim_end_matches(".git")
            .split('/')
            .filter(|part| !part.is_empty());
        let (owner, repo) = (parts.next()?, parts.next()?);
        if host.is_empty() || owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(Self(host.to_ascii_lowercase()))
    }

    fn owns(&self, url: &str) -> bool {
        let Some((host, path)) = url
            .strip_prefix(URL_PREFIX)
            .and_then(|rest| rest.split_once('/'))
        else {
            return false;
        };
        let parts: Vec<&str> = path.split('/').collect();
        host.eq_ignore_ascii_case(&self.0)
            && parts.len() == PULL_PATH_PARTS
            && parts[PULL_WORD_INDEX] == PULL_WORD
            && parts[..PULL_WORD_INDEX].iter().all(|part| !part.is_empty())
            && !parts[PULL_PATH_PARTS - 1].is_empty()
            && parts[PULL_PATH_PARTS - 1]
                .bytes()
                .all(|b| b.is_ascii_digit())
    }
}

/// The host of the remote `origin` in the folder `cwd`, or `None` when there is no such
/// remote or it is not in a form [`OriginHost::parse`] reads.
pub(crate) fn origin_host(tools: &ShipTools, cwd: &Path) -> Option<OriginHost> {
    let remote = run(&tools.git, cwd, &["remote", "get-url", REMOTE], GIT_LIMIT).ok()?;
    OriginHost::parse(&remote)
}

/// The pull request address in `output` that is on `origin`'s host: the last line that is a
/// pull request address there, so output that names another host is not taken.
pub(crate) fn printed_pull_request_of(output: &str, origin: &OriginHost) -> Option<String> {
    printed_pull_request_matching(output, |line| origin.owns(line))
}

/// Whether the output of a shell tool call ended without a failure exit status.
pub(crate) fn ended_cleanly(output: &str) -> bool {
    !output.contains(EXIT_STATUS_MARKER)
}

/// The pull request address `gh pr create` printed in `output`, if it printed one.
pub(crate) fn printed_pull_request(output: &str) -> Option<String> {
    printed_pull_request_matching(output, |line| line.contains(PULL_PATH))
}

fn printed_pull_request_matching(output: &str, wanted: impl Fn(&str) -> bool) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .rev()
        .find(|line| {
            line.starts_with(URL_PREFIX)
                && wanted(line)
                && line.len() <= MAX_URL_BYTES
                && !line.chars().any(|c| c.is_control() || c.is_whitespace())
        })
        .map(str::to_owned)
}

fn run(program: &OsString, cwd: &Path, args: &[&str], limit: Duration) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    for name in INHERITED_GIT_VARIABLES {
        command.env_remove(name);
    }
    for (name, value) in NO_PROMPT_VARIABLES {
        command.env(name, value);
    }
    let name = program.to_string_lossy();
    let done = run_bounded(&mut command, limit).map_err(|e| format!("{name}: {e}"))?;
    if done.status.success() {
        return Ok(done.stdout);
    }
    let reason = done.stderr.trim();
    Err(if reason.is_empty() {
        format!("{name} {} failed with {}", args.join(" "), done.status)
    } else {
        format!(
            "{name} {} failed: {reason}",
            args.first().copied().unwrap_or_default()
        )
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use coda_protocol::types::SessionId;
    use tempfile::TempDir;

    use super::*;

    const PR_URL: &str = "https://github.com/o/r/pull/7";

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    struct Rig {
        _dir: TempDir,
        work: PathBuf,
        remote: PathBuf,
        gh_log: PathBuf,
        tools: ShipTools,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        fs::create_dir(&remote).unwrap();
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        let work = dir.path().join("work");
        fs::create_dir(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["config", "user.name", "t"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "commit.gpgsign", "false"]);
        git(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        fs::write(work.join("a.txt"), "one\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "first"]);
        git(&work, &["checkout", "-q", "-b", "coda/1"]);
        let gh_log = dir.path().join("gh.log");
        let script = dir.path().join("gh");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> '{}'; done\necho 'Creating pull request'\necho '{PR_URL}'\n",
                gh_log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let tools = ShipTools {
            git: OsString::from("git"),
            gh: script.into_os_string(),
        };
        Rig {
            _dir: dir,
            work,
            remote,
            gh_log,
            tools,
        }
    }

    fn params(action: ShipAction) -> SessionShipParams {
        SessionShipParams {
            session_id: SessionId::new("1"),
            action,
            message: "fix(parser): keep the last line\n\nmore".to_owned(),
            body: Some("because it was lost".to_owned()),
            ready: false,
            all: true,
        }
    }

    #[test]
    fn the_address_gh_prints_last_is_the_pull_request() {
        // Arrange
        let output = "Creating pull request for a into main\n\nhttps://github.com/o/r/pull/12\n";

        // Act
        let found = printed_pull_request(output);

        // Assert
        assert_eq!(found.as_deref(), Some("https://github.com/o/r/pull/12"));
    }

    #[test]
    fn output_without_a_pull_request_address_has_none() {
        // Arrange
        let output = "see https://github.com/o/r/issues/3\nhttps://example.com/other\n";

        // Act
        let found = printed_pull_request(output);

        // Assert
        assert_eq!(found, None);
    }

    #[test]
    fn a_gh_pr_create_named_only_in_a_string_or_another_command_does_not_count() {
        // Arrange
        let opens = [
            "gh pr create --fill",
            "git push && gh pr create --fill",
            "cd sub; gh pr create --title 'a;b'",
            "FOO=1 gh pr create",
            "git push\ngh pr create",
        ];
        let unread = [
            "echo \"x\\\"; gh pr create; echo \\\"\"",
            "cat <<EOF\ngh pr create\nEOF",
            "(cd x && gh pr create)",
            "$(gh pr create)",
            "`gh pr create`",
            "{ gh pr create; }",
            "echo https://github.com/acme/app/pull/3 # ; gh pr create",
            "gh pr create --title 'fix (parser)'",
        ];
        let others = [
            "echo \"gh pr create\"; cat file",
            "echo 'a; gh pr create'",
            "cat notes | grep \"gh pr create\"",
            "gh pr created",
            "gh pr create-x",
            "echo gh pr create",
            "git commit -m 'gh pr create'",
        ];

        // Act and Assert
        for command in opens {
            assert!(creates_pull_request(command), "{command}");
        }
        for command in others.iter().chain(unread.iter()) {
            assert!(!creates_pull_request(command), "{command}");
        }
    }

    fn origin() -> OriginHost {
        OriginHost::parse("git@github.com:Acme/App.git").unwrap()
    }

    #[test]
    fn an_origin_remote_reads_as_a_host_in_every_common_form() {
        // Arrange
        let forms = [
            "https://github.com/Acme/App.git",
            "https://github.com/Acme/App",
            "https://user@github.com/Acme/App.git",
            "ssh://git@github.com/Acme/App.git",
            "git@github.com:Acme/App.git",
            "https://GitHub.com/acme/app/",
            "ssh://git@github.com:22/Acme/App.git",
            "git://github.com/Acme/App.git",
        ];

        // Act and Assert
        for form in forms {
            assert_eq!(OriginHost::parse(form), Some(origin()), "{form}");
        }
        assert_eq!(
            OriginHost::parse("https://ghe.example:8443/a/b.git"),
            OriginHost::parse("https://ghe.example:8443/c/d")
        );
        for bad in [
            "",
            "/srv/repos/app.git",
            "file:///x/y.git",
            "https://github.com/onlyowner",
        ] {
            assert_eq!(OriginHost::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_a_pull_request_on_the_origin_host_is_taken() {
        // Arrange
        let cases = [
            (
                "https://github.com/acme/app/pull/7",
                Some("https://github.com/acme/app/pull/7"),
            ),
            (
                "noise\nhttps://GitHub.com/Acme/App/pull/12\n",
                Some("https://GitHub.com/Acme/App/pull/12"),
            ),
            ("https://evil.example/acme/app/pull/7", None),
            (
                "https://github.com/someone/fork/pull/7",
                Some("https://github.com/someone/fork/pull/7"),
            ),
            ("https://github.com/acme/pull/7", None),
            ("https://github.com/acme/app/pull/7/files", None),
            ("https://github.com/acme/app/pull/x", None),
            ("https://github.com.evil.example/acme/app/pull/7", None),
        ];

        // Act and Assert
        for (output, want) in cases {
            assert_eq!(
                printed_pull_request_of(output, &origin()).as_deref(),
                want,
                "{output}"
            );
        }
    }

    #[test]
    fn only_a_gh_pr_create_command_is_one_that_opens_a_pull_request() {
        // Arrange
        let opens = "git push && gh pr create --fill";
        let others = ["gh pr view 3", "gh pr list", "echo gh pr"];

        // Act and Assert
        assert!(creates_pull_request(opens));
        for command in others {
            assert!(!creates_pull_request(command), "{command}");
        }
    }

    #[test]
    fn commit_stages_everything_and_commits_with_the_message_and_does_not_push() {
        // Arrange
        let rig = rig();
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();
        fs::write(rig.work.join("new.txt"), "new\n").unwrap();

        // Act
        let done = ship(&rig.tools, &rig.work, &params(ShipAction::Commit)).unwrap();

        // Assert
        assert!(!done.pushed);
        assert_eq!(done.pull_request, None);
        assert_eq!(done.branch, "coda/1");
        assert_eq!(done.commit, git(&rig.work, &["rev-parse", "HEAD"]).trim());
        assert_eq!(git(&rig.work, &["status", "--porcelain"]), "");
        assert!(git(&rig.work, &["log", "-1", "--format=%B"]).starts_with("fix(parser)"));
        assert_eq!(git(&rig.remote, &["branch", "--list"]).trim(), "");
    }

    #[test]
    fn without_all_only_the_staged_files_are_committed_and_listed() {
        // Arrange
        let rig = rig();
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();
        fs::write(rig.work.join("staged.txt"), "staged\n").unwrap();
        fs::write(rig.work.join(".env"), "SECRET=1\n").unwrap();
        git(&rig.work, &["add", "staged.txt"]);
        let mut asked = params(ShipAction::Commit);
        asked.all = false;

        // Act
        let done = ship(&rig.tools, &rig.work, &asked).unwrap();

        // Assert
        assert_eq!(done.files, ["staged.txt"]);
        assert_eq!(
            git(&rig.work, &["show", "--name-only", "--format=", "HEAD"]).trim(),
            "staged.txt"
        );
        assert_eq!(
            git(&rig.work, &["status", "--porcelain"]),
            " M a.txt\n?? .env\n"
        );
    }

    #[test]
    fn without_all_and_nothing_staged_is_refused_and_leaves_the_tree_alone() {
        // Arrange
        let rig = rig();
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();
        let mut asked = params(ShipAction::CommitPush);
        asked.all = false;

        // Act
        let refused = ship(&rig.tools, &rig.work, &asked).unwrap_err();

        // Assert
        assert!(refused.contains("stage files first"), "{refused}");
        assert_eq!(git(&rig.work, &["status", "--porcelain"]), " M a.txt\n");
        assert_eq!(git(&rig.remote, &["branch", "--list"]).trim(), "");
    }

    #[test]
    fn all_stages_every_change_and_lists_the_files() {
        // Arrange
        let rig = rig();
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();
        fs::write(rig.work.join("new.txt"), "new\n").unwrap();
        fs::write(rig.work.join("odd name\u{e9}.txt"), "x\n").unwrap();

        // Act
        let done = ship(&rig.tools, &rig.work, &params(ShipAction::Commit)).unwrap();

        // Assert
        assert_eq!(done.files, ["a.txt", "new.txt", "odd name\u{e9}.txt"]);
    }

    #[test]
    fn commit_push_sends_the_branch_to_origin_and_opens_no_pull_request() {
        // Arrange
        let rig = rig();
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();

        // Act
        let done = ship(&rig.tools, &rig.work, &params(ShipAction::CommitPush)).unwrap();

        // Assert
        assert!(done.pushed);
        assert_eq!(done.pull_request, None);
        assert_eq!(
            git(&rig.remote, &["rev-parse", "refs/heads/coda/1"]).trim(),
            done.commit
        );
        assert!(!rig.gh_log.exists());
    }

    #[test]
    fn commit_push_pr_opens_a_draft_with_the_first_line_as_title() {
        // Arrange
        let rig = rig();
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();

        // Act
        let done = ship(&rig.tools, &rig.work, &params(ShipAction::CommitPushPr)).unwrap();

        // Assert
        assert_eq!(done.pull_request.as_deref(), Some(PR_URL));
        let args = fs::read_to_string(&rig.gh_log).unwrap();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                "pr",
                "create",
                "--title",
                "fix(parser): keep the last line",
                "--body",
                "because it was lost",
                "--draft"
            ]
        );
    }

    #[test]
    fn ready_leaves_out_the_draft_flag() {
        // Arrange
        let rig = rig();
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();
        let mut asked = params(ShipAction::CommitPushPr);
        asked.ready = true;

        // Act
        ship(&rig.tools, &rig.work, &asked).unwrap();

        // Assert
        assert!(!fs::read_to_string(&rig.gh_log).unwrap().contains("--draft"));
    }

    #[test]
    fn nothing_to_commit_is_refused_and_pushes_nothing() {
        // Arrange
        let rig = rig();

        // Act
        let refused = ship(&rig.tools, &rig.work, &params(ShipAction::CommitPush)).unwrap_err();

        // Assert
        assert!(refused.contains("nothing to commit"), "{refused}");
        assert_eq!(git(&rig.remote, &["branch", "--list"]).trim(), "");
    }

    #[test]
    fn pushing_the_default_branch_is_refused_before_anything_is_committed() {
        // Arrange
        let rig = rig();
        git(&rig.work, &["checkout", "-q", "main"]);
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();

        // Act
        let refused = ship(&rig.tools, &rig.work, &params(ShipAction::CommitPush)).unwrap_err();

        // Assert
        assert!(refused.contains("default branch"), "{refused}");
        assert_ne!(git(&rig.work, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_commit_alone_on_the_default_branch_is_allowed() {
        // Arrange
        let rig = rig();
        git(&rig.work, &["checkout", "-q", "main"]);
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();

        // Act
        let done = ship(&rig.tools, &rig.work, &params(ShipAction::Commit)).unwrap();

        // Assert
        assert_eq!(done.branch, "main");
    }

    #[test]
    fn an_address_with_control_characters_or_spaces_is_not_taken() {
        // Arrange
        let outputs = [
            "https://github.com/o/r/pull/1\u{1b}]0;x",
            "https://github.com/o/r/pull/1 extra",
        ];

        // Act and Assert
        for output in outputs {
            assert_eq!(printed_pull_request(output), None, "{output:?}");
        }
    }

    #[test]
    fn output_with_a_failure_exit_status_did_not_end_cleanly() {
        // Arrange and Act and Assert
        assert!(ended_cleanly("done\n"));
        assert!(!ended_cleanly("exists\n\n[exit status 1]"));
    }

    #[test]
    fn an_empty_message_is_refused_before_git_runs() {
        // Arrange
        let rig = rig();
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();
        let mut asked = params(ShipAction::Commit);
        asked.message = "  \n".to_owned();

        // Act
        let refused = ship(&rig.tools, &rig.work, &asked).unwrap_err();

        // Assert
        assert!(refused.contains("empty"), "{refused}");
        assert_ne!(git(&rig.work, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_detached_head_is_refused() {
        // Arrange
        let rig = rig();
        git(&rig.work, &["checkout", "-q", "--detach"]);
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();

        // Act
        let refused = ship(&rig.tools, &rig.work, &params(ShipAction::Commit)).unwrap_err();

        // Assert
        assert!(refused.contains("not on a branch"), "{refused}");
    }

    #[test]
    fn a_push_the_remote_rejects_is_reported_and_leaves_the_commit() {
        // Arrange
        let rig = rig();
        git(
            &rig.work,
            &["remote", "set-url", "origin", "/nonexistent/remote.git"],
        );
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();

        // Act
        let refused = ship(&rig.tools, &rig.work, &params(ShipAction::CommitPush)).unwrap_err();

        // Assert
        assert!(refused.contains("push"), "{refused}");
        assert_eq!(git(&rig.work, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_gh_that_prints_no_address_is_an_error() {
        // Arrange
        let mut rig = rig();
        let silent = rig.work.parent().unwrap().join("silent-gh");
        fs::write(&silent, "#!/bin/sh\necho done\n").unwrap();
        fs::set_permissions(&silent, fs::Permissions::from_mode(0o755)).unwrap();
        rig.tools.gh = silent.into_os_string();
        fs::write(rig.work.join("a.txt"), "two\n").unwrap();

        // Act
        let refused = ship(&rig.tools, &rig.work, &params(ShipAction::CommitPushPr)).unwrap_err();

        // Assert
        assert!(refused.contains("address"), "{refused}");
    }
}
