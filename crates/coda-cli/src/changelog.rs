// SPDX-License-Identifier: Apache-2.0

//! `coda changelog`: one proposed entry for the `Unreleased` section of the changelog, shown
//! as a diff. The file is written only with `--write` and the user's confirmation, and the
//! command never commits.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use coda_protocol::types::BackendKind;
use coda_review::changelog::{
    BadProposal, Inserted, MAX_COMMITS, Proposal, UNRELEASED_HEADING, has_entry, has_unreleased,
    insert, parse_proposal, preview, prompt, unreleased_entries,
};
use coda_review::diff::{commit_messages, current_branch, parse_diff};
use coda_review::request::{Role, Target};
use serde::Serialize;

use crate::autostart::ensure_daemon;
use crate::cli::ChangelogArgs;
use crate::context::{Context, Io};
use crate::daemon_cmd::load_config;
use crate::error::CliError;
use crate::review::{Ask, change_text, default_branch, load_rules, review_backend, split_files};
use crate::review_post::{Confirm, asked_yes};
use crate::review_route::Attempt;
use crate::run::session_cwd;

/// Starts the name of the file a model-API run reads.
const FILE_PREFIX: &str = "changelog";
/// An entry is asked for again once when its answer cannot be used.
const MAX_ATTEMPTS: usize = 2;
/// Entries of the file shown to the scribe as examples of its style.
const EXAMPLE_ENTRIES: usize = 5;
const HEAD: &str = "HEAD";
const PR_PREFIX: &str = "pr-";
const PR_URL_SEGMENT: &str = "/pull/";
const NOTHING_TO_WRITE: &str = "nothing to write: the change has no lines to look at";
const NOTHING_LEFT: &str = "nothing to write: every changed file was left out by the review rules";
const WRITE_QUESTION: &str = "Write this entry to the file? [y/N] ";

/// What `--json` prints.
#[derive(Serialize)]
struct Output<'a> {
    section: &'a str,
    entry: &'a str,
    line: usize,
    diff: &'a str,
}

/// Runs the command.
///
/// # Errors
/// Returns a [`CliError`] when the file cannot be read or has no `Unreleased` heading, the
/// change cannot be read, the scribe fails or does not answer in the format twice, or the file
/// changed while the entry was drafted.
pub async fn run(
    ctx: &Context,
    io: &mut Io<'_>,
    args: ChangelogArgs,
) -> Result<ExitCode, CliError> {
    let config = load_config(&ctx.dirs)?;
    let backend = review_backend(&config, args.backend.map(Into::into));
    let cwd = session_cwd(&ctx.cwd, args.cwd.clone())?;
    let repo = Path::new(&cwd);
    let target = (!args.staged).then(|| Target::parse(args.target.as_deref()));
    let confirm = Confirm {
        assume_yes: args.yes,
        interactive: ctx.stdin_is_tty,
    };
    if args.write {
        confirm.check().map_err(local)?;
    }
    let path = repo.join(&args.file);
    let text = read_file(&path)?;
    let id = branch_id(repo, target.as_ref())?;
    if has_entry(&text, &id) {
        say(
            io,
            args.json,
            "already_there",
            &format!("the changelog already has an entry for `{id}`: nothing to add"),
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    if !has_unreleased(&text) {
        return Err(CliError::Changelog(format!(
            "the changelog has no `{UNRELEASED_HEADING}` heading to add the entry under"
        )));
    }
    let rules = load_rules(&config.review, repo)?;
    let files = parse_diff(&change_text(repo, target.as_ref())?);
    if files.is_empty() {
        say(io, args.json, "nothing_to_write", NOTHING_TO_WRITE)?;
        return Ok(ExitCode::SUCCESS);
    }
    let parts = split_files(repo, &files, &config.review, &rules)?;
    let part = &parts[0];
    if part.coverage.reviewed.is_empty() && part.coverage.clipped.is_empty() {
        say(io, args.json, "nothing_to_write", NOTHING_LEFT)?;
        return Ok(ExitCode::SUCCESS);
    }
    if parts.len() > 1 || !part.coverage.left_out.is_empty() {
        writeln!(
            io.err,
            "coda: the entry is from the first part of a large change"
        )?;
    }
    let commits = commits_of(repo, target.as_ref())?;
    let examples: Vec<String> = unreleased_entries(&text)
        .into_iter()
        .take(EXAMPLE_ENTRIES)
        .collect();
    let input = prompt(
        &part.text,
        &commits,
        &examples,
        args.instructions.as_deref(),
    );
    drop(ensure_daemon(ctx, io, false).await?);
    let mut asker = Ask {
        ctx,
        cwd: &cwd,
        role: Role::Scribe,
        file_prefix: FILE_PREFIX,
        input: &input,
    };
    let proposal = ask_proposal(&mut asker, backend).await?;
    let placed = insert(&text, &proposal, &id).map_err(|e| CliError::Changelog(e.to_string()))?;
    let Inserted::New {
        text: new_text,
        line,
    } = placed
    else {
        return Ok(ExitCode::SUCCESS);
    };
    let name = args.file.display().to_string();
    let diff = preview(&name, &text, &new_text);
    print_proposal(io, &proposal, line, &diff, args.json)?;
    if args.write {
        write_entry(io, confirm, &path, &text, &new_text)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Says why nothing is proposed: a line, or with `--json` an object with a `status`.
fn say(io: &mut Io<'_>, json: bool, status: &str, text: &str) -> Result<(), CliError> {
    if json {
        let line = serde_json::json!({"status": status, "message": text});
        writeln!(io.out, "{line}")?;
    } else {
        writeln!(io.out, "{text}")?;
    }
    Ok(())
}

/// A changelog error said as this command's.
fn local(error: CliError) -> CliError {
    match error {
        CliError::ReviewPost(why) => CliError::Changelog(why),
        other => other,
    }
}

fn read_file(path: &Path) -> Result<String, CliError> {
    fs::read_to_string(path)
        .map_err(|e| CliError::Changelog(format!("cannot read {}: {e}", path.display())))
}

/// The name that marks the entry of this change: the branch, or the pull request.
fn branch_id(repo: &Path, target: Option<&Target>) -> Result<String, CliError> {
    let named = match target {
        Some(Target::PullRequest(pr)) => return Ok(format!("{PR_PREFIX}{}", pr_number(pr))),
        Some(Target::Branch(branch)) => Some(branch.as_str()),
        Some(Target::Range { head, .. }) => Some(head.as_str()),
        Some(Target::CurrentBranch) | None => None,
    };
    if let Some(name) = named.filter(|n| *n != HEAD) {
        return Ok(name.to_owned());
    }
    current_branch(repo)
        .map_err(|e| CliError::Changelog(e.to_string()))?
        .ok_or_else(|| {
            CliError::Changelog(
                "no branch is checked out to name the entry after; name one as the target"
                    .to_owned(),
            )
        })
}

/// The number of a pull request given as a number or as its URL, so both name the same entry.
fn pr_number(pr: &str) -> &str {
    let rest = pr.split_once(PR_URL_SEGMENT).map_or(pr, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// The commit messages of the change, for the targets that are a range of commits.
fn commits_of(repo: &Path, target: Option<&Target>) -> Result<Vec<String>, CliError> {
    let (base, head) = match target {
        Some(Target::Range { base, head }) => (base.clone(), head.clone()),
        Some(Target::CurrentBranch) => (default_branch(repo)?, HEAD.to_owned()),
        Some(Target::Branch(branch)) => (default_branch(repo)?, branch.clone()),
        Some(Target::PullRequest(_)) | None => return Ok(Vec::new()),
    };
    commit_messages(repo, &base, &head, MAX_COMMITS)
        .map_err(|e| CliError::ReviewInput(e.to_string()))
}

/// Asks the scribe until its answer is a usable proposal, at most [`MAX_ATTEMPTS`] times. The
/// second ask carries the hint that says what was wrong.
async fn ask_proposal(
    asker: &mut impl Attempt,
    backend: BackendKind,
) -> Result<Proposal, CliError> {
    let mut hint: Option<String> = None;
    let mut last = BadProposal(String::new());
    for _ in 0..MAX_ATTEMPTS {
        let answer = asker.attempt(backend, None, hint.take()).await?;
        match parse_proposal(&answer.text) {
            Ok(proposal) => return Ok(proposal),
            Err(bad) => {
                hint = Some(bad.retry_hint());
                last = bad;
            }
        }
    }
    Err(CliError::Changelog(format!(
        "the scribe did not give a usable answer, so nothing was written: {last}"
    )))
}

fn print_proposal(
    io: &mut Io<'_>,
    proposal: &Proposal,
    line: usize,
    diff: &str,
    json: bool,
) -> Result<(), CliError> {
    if json {
        let output = Output {
            section: proposal.section.name(),
            entry: &proposal.entry,
            line,
            diff,
        };
        let text = serde_json::to_string_pretty(&output)
            .map_err(|e| CliError::Changelog(e.to_string()))?;
        writeln!(io.out, "{text}")?;
        return Ok(());
    }
    writeln!(
        io.out,
        "Section: {}\nEntry: {}\n\n{diff}",
        proposal.section.name(),
        proposal.entry
    )?;
    Ok(())
}

/// Writes `new_text` over the file after a preview and a yes, if the file still holds
/// `original`.
fn write_entry(
    io: &mut Io<'_>,
    confirm: Confirm,
    path: &Path,
    original: &str,
    new_text: &str,
) -> Result<(), CliError> {
    if !confirm.assume_yes && !asked_yes(io, WRITE_QUESTION)? {
        writeln!(io.err, "coda: nothing was written")?;
        return Ok(());
    }
    if read_file(path)? != original {
        return Err(CliError::Changelog(format!(
            "{} changed while the entry was drafted, so nothing was written; run the command again",
            path.display()
        )));
    }
    fs::write(path, new_text)
        .map_err(|e| CliError::Changelog(format!("cannot write {}: {e}", path.display())))?;
    writeln!(io.err, "coda: entry written to {}", path.display())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io::Cursor;

    use super::*;
    use crate::review::Answer;

    const FILE: &str = "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- Old.\n\n## [0.1.0]\n";
    const GOOD: &str = r#"{"section": "Fixed", "entry": "The loop ends."}"#;
    const YES: Confirm = Confirm {
        assume_yes: true,
        interactive: false,
    };
    const ASK: Confirm = Confirm {
        assume_yes: false,
        interactive: true,
    };

    struct Streams {
        out: Vec<u8>,
        err: Vec<u8>,
        input: Cursor<Vec<u8>>,
    }

    impl Streams {
        fn new(answer: &str) -> Self {
            Self {
                out: Vec::new(),
                err: Vec::new(),
                input: Cursor::new(answer.as_bytes().to_vec()),
            }
        }

        fn io(&mut self) -> Io<'_> {
            Io {
                out: &mut self.out,
                err: &mut self.err,
                input: &mut self.input,
            }
        }
    }

    struct Scripted {
        replies: Vec<&'static str>,
        hints: Vec<Option<String>>,
    }

    impl Attempt for Scripted {
        fn attempt(
            &mut self,
            _: BackendKind,
            _: Option<&str>,
            hint: Option<String>,
        ) -> impl Future<Output = Result<Answer, CliError>> {
            self.hints.push(hint);
            std::future::ready(Ok(Answer {
                text: self.replies.remove(0).to_owned(),
                model: None,
            }))
        }
    }

    fn with_file(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("CHANGELOG.md");
        fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn new_text() -> String {
        let proposal = parse_proposal(GOOD).unwrap();
        match insert(FILE, &proposal, "feat/x").unwrap() {
            Inserted::New { text, .. } => text,
            Inserted::Already => panic!("already"),
        }
    }

    #[tokio::test]
    async fn a_bad_answer_is_asked_for_again_once_and_a_second_one_is_an_error() {
        // Arrange
        let mut recovers = Scripted {
            replies: vec!["prose", GOOD],
            hints: Vec::new(),
        };
        let mut fails = Scripted {
            replies: vec!["prose", r#"{"section": "Fixed", "entry": " "}"#, GOOD],
            hints: Vec::new(),
        };

        // Act
        let good = ask_proposal(&mut recovers, BackendKind::Faux).await;
        let bad = ask_proposal(&mut fails, BackendKind::Faux).await;

        // Assert
        assert_eq!(good.unwrap().entry, "The loop ends.");
        assert!(recovers.hints[0].is_none());
        assert!(recovers.hints[1].as_deref().unwrap().contains("rejected"));
        let error = bad.unwrap_err().to_string();
        assert!(error.contains("nothing was written"), "{error}");
        assert_eq!(fails.replies.len(), 1, "no third ask");
    }

    #[test]
    fn an_unreadable_file_is_refused() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let bytes = dir.path().join("bytes.md");
        fs::write(&bytes, [0xff, 0xfe, 0x00]).unwrap();

        // Act
        let missing = read_file(&dir.path().join("none.md")).unwrap_err();
        let not_text = read_file(&bytes).unwrap_err();

        // Assert
        assert!(missing.to_string().contains("cannot read"), "{missing}");
        assert!(not_text.to_string().contains("cannot read"), "{not_text}");
    }

    #[test]
    fn a_confirmed_write_puts_the_entry_in_the_file() {
        // Arrange
        let (_dir, path) = with_file(FILE);
        let mut streams = Streams::new("");

        // Act
        write_entry(&mut streams.io(), YES, &path, FILE, &new_text()).unwrap();

        // Assert
        assert_eq!(fs::read_to_string(&path).unwrap(), new_text());
        assert!(String::from_utf8_lossy(&streams.err).contains("entry written"));
    }

    #[test]
    fn a_no_writes_nothing_and_the_question_is_asked() {
        // Arrange
        let (_dir, path) = with_file(FILE);
        let mut streams = Streams::new("n\n");

        // Act
        write_entry(&mut streams.io(), ASK, &path, FILE, &new_text()).unwrap();

        // Assert
        assert_eq!(fs::read_to_string(&path).unwrap(), FILE);
        let err = String::from_utf8_lossy(&streams.err);
        assert!(err.contains("Write this entry"), "{err}");
        assert!(err.contains("nothing was written"), "{err}");
    }

    #[test]
    fn a_file_that_changed_since_it_was_read_is_not_written() {
        // Arrange
        let (_dir, path) = with_file("someone else wrote this\n");
        let mut streams = Streams::new("");

        // Act
        let error = write_entry(&mut streams.io(), YES, &path, FILE, &new_text()).unwrap_err();

        // Assert
        assert!(error.to_string().contains("changed while"), "{error}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "someone else wrote this\n"
        );
    }

    #[test]
    fn the_id_is_the_pull_request_or_the_named_branch() {
        // Arrange
        let dir = Path::new(".");
        let range = Target::Range {
            base: "main".to_owned(),
            head: "topic".to_owned(),
        };

        // Act and Assert
        assert_eq!(
            branch_id(dir, Some(&Target::PullRequest("7".to_owned()))).unwrap(),
            "pr-7"
        );
        assert_eq!(
            branch_id(dir, Some(&Target::Branch("feat/a".to_owned()))).unwrap(),
            "feat/a"
        );
        assert_eq!(branch_id(dir, Some(&range)).unwrap(), "topic");
        for same in [
            "7",
            "https://github.com/o/r/pull/7",
            "https://github.com/o/r/pull/7/files?x=1",
        ] {
            let pr = Target::PullRequest(same.to_owned());
            assert_eq!(branch_id(dir, Some(&pr)).unwrap(), "pr-7", "{same}");
        }
    }

    #[test]
    fn a_json_run_that_has_nothing_to_propose_still_prints_json() {
        // Arrange
        let mut streams = Streams::new("");

        // Act
        say(&mut streams.io(), true, "already_there", "same \"x\"").unwrap();

        // Assert
        let parsed: serde_json::Value = serde_json::from_slice(&streams.out).unwrap();
        assert_eq!(parsed["status"], "already_there");
        assert_eq!(parsed["message"], "same \"x\"");
    }
}
