// SPDX-License-Identifier: Apache-2.0

//! `coda review`: packs a change, has the `reviewer` agent read it through the `task` tool
//! and prints the checked findings. The command only reads: it posts nothing and edits no
//! file of the repository.
//!
//! The packed diff is too large to ask the model to copy into a tool call, so it is written
//! to a short-lived file in the working directory, and the reviewer is told to read it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use coda_config::config::Config;
use coda_config::review::ReviewConfig;
use coda_core::history::{ASSISTANT_MESSAGE_KIND, CHILD_EVENT_KIND};
use coda_core::turn::StopCondition;
use coda_protocol::types::{BackendKind, Mode};
use coda_review::diff::{FileDiff, git_diff, git_diff_staged, parse_diff, tracked_files};
use coda_review::pack::{PackOptions, Packed, main_extension, pack};
use coda_review::parse::{BadShape, Parsed, parse_model_output};
use coda_review::render::{to_json, to_markdown};
use coda_review::request::{
    GH_PR_FIELDS, REVIEWER_AGENT, Target, assemble, input_text, pr_info_from_gh, session_prompt,
    task_prompt,
};
use coda_review::rules::{Rules, default_branch_ref, load};
use coda_review::skip::Decision;
use serde_json::Value;
use tracing::info;

use crate::cli::ReviewArgs;
use crate::context::{Context, Io};
use crate::daemon_cmd::load_config;
use crate::error::CliError;
use crate::run::{Job, Tap, drive, pick_backend, session_cwd, stop_reason};

/// Where the packed input is written, relative to the working directory.
const INPUT_DIR: &str = ".coda/review";
const INPUT_PARENT_DIR: &str = ".coda";
/// A review asks the reviewer again once when its answer has the wrong shape.
const MAX_ATTEMPTS: usize = 2;
const NOTHING_TO_REVIEW: &str = "nothing to review: the change has no lines to look at";
const NOTHING_LEFT: &str = "nothing to review: every changed file was left out by the review rules";
const FORCED_NOTE: &str = "a skip rule matched, but --force asks for the review anyway";
const GH: &str = "gh";
const HEAD: &str = "HEAD";
const END_TURN: &str = StopCondition::EndTurn.as_str();

/// Runs the review. `force` reviews a pull request that a skip rule would skip.
///
/// # Errors
/// Returns a [`CliError`] when the change cannot be read, the backend runs its own agent
/// loop, the run fails, or the reviewer does not answer in the review format twice.
pub async fn run(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    args: ReviewArgs,
) -> Result<ExitCode, CliError> {
    let config = load_config(&ctx.dirs)?;
    let backend = review_backend(&config, args.backend.map(Into::into))?;
    let cwd = session_cwd(&ctx.cwd, args.cwd.clone())?;
    let repo = Path::new(&cwd);
    info!(step = "read_change", "reading the change");
    let rules = load_rules(&config.review, repo)?;
    let target = if args.staged {
        None
    } else {
        Some(Target::parse(args.target.as_deref()))
    };
    if let Some(Target::PullRequest(pr)) = &target {
        match gh_pr_decision(repo, pr, &rules, force)? {
            Decision::Skip(reason) => {
                writeln!(io.out, "{}", reason.one_line())?;
                return Ok(ExitCode::SUCCESS);
            }
            Decision::Forced(_) => writeln!(io.err, "coda: {FORCED_NOTE}")?,
            Decision::Review => {}
        }
    }
    let diff = change_text(repo, target.as_ref())?;
    let files = parse_diff(&diff);
    if files.is_empty() {
        writeln!(io.out, "{NOTHING_TO_REVIEW}")?;
        return Ok(ExitCode::SUCCESS);
    }
    let packed = pack_files(repo, &files, &config.review, &rules)?;
    if packed.coverage.reviewed.is_empty() && packed.coverage.clipped.is_empty() {
        writeln!(io.out, "{NOTHING_LEFT}")?;
        return Ok(ExitCode::SUCCESS);
    }
    let input = input_text(
        &packed,
        args.instructions.as_deref(),
        config.review.max_findings,
    );
    let file = InputFile::write(repo, &input)?;
    let mut reviewer = Live {
        ctx,
        io,
        force,
        backend,
        cwd: &cwd,
        input: &file.relative,
    };
    let (parsed, model) = until_usable(&packed, config.review.max_findings, &mut reviewer).await?;
    let model = model.unwrap_or_else(|| backend_name(backend));
    let review = assemble(parsed, &packed, &model, input.chars().count());
    if args.json {
        write!(io.out, "{}", to_json(&review)?)?;
    } else {
        write!(io.out, "{}", to_markdown(&review))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// The backend that runs the review: the one asked for, else the first model backend the
/// config enables, else the Anthropic API. A backend that runs its own agent loop has no
/// `task` tool of Coda's, so it is refused.
fn review_backend(config: &Config, asked: Option<BackendKind>) -> Result<BackendKind, CliError> {
    let backend = asked.unwrap_or_else(|| {
        let picked = pick_backend(config);
        if picked.is_model_backend() {
            picked
        } else {
            BackendKind::Anthropic
        }
    });
    if backend.is_model_backend() {
        Ok(backend)
    } else {
        Err(CliError::ReviewBackend {
            backend: backend_name(backend),
        })
    }
}

/// The name of `backend` as the config and the protocol write it.
fn backend_name(backend: BackendKind) -> String {
    serde_json::to_value(backend)
        .ok()
        .and_then(|name| name.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{backend:?}"))
}

fn load_rules(config: &ReviewConfig, repo: &Path) -> Result<Rules, CliError> {
    let rules = match default_branch_ref(repo) {
        Some(default_ref) => load(config, repo, &default_ref),
        None => Rules::compile(config),
    };
    rules.map_err(|e| CliError::ReviewInput(e.to_string()))
}

/// Runs `gh` in `repo` and returns what it printed.
fn gh(repo: &Path, args: &[&str]) -> Result<String, CliError> {
    let out = Command::new(GH)
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| CliError::ReviewInput(format!("could not run `{GH}`: {e}")))?;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        return Err(CliError::ReviewInput(format!("`{GH}` failed: {why}")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn gh_pr_decision(repo: &Path, pr: &str, rules: &Rules, force: bool) -> Result<Decision, CliError> {
    let json = gh(repo, &["pr", "view", pr, "--json", GH_PR_FIELDS])?;
    let info = pr_info_from_gh(&json).map_err(|e| CliError::ReviewInput(e.to_string()))?;
    Ok(rules.skip.decide(&info, force))
}

/// The unified diff of the change. `None` is the staged changes.
fn change_text(repo: &Path, target: Option<&Target>) -> Result<String, CliError> {
    let read = |result: Result<String, coda_review::diff::GitError>| {
        result.map_err(|e| CliError::ReviewInput(e.to_string()))
    };
    match target {
        None => read(git_diff_staged(repo)),
        Some(Target::PullRequest(pr)) => gh(repo, &["pr", "diff", pr, "--color", "never"]),
        Some(Target::Range { base, head }) => read(git_diff(repo, base, head)),
        Some(Target::CurrentBranch) => read(git_diff(repo, &default_branch(repo)?, HEAD)),
        Some(Target::Branch(branch)) => read(git_diff(repo, &default_branch(repo)?, branch)),
    }
}

fn default_branch(repo: &Path) -> Result<String, CliError> {
    default_branch_ref(repo).ok_or_else(|| {
        CliError::ReviewInput(
            "the repository has no default branch (origin/HEAD, main or master) to compare with; \
             name a range such as `main...topic`"
                .to_owned(),
        )
    })
}

fn pack_files(
    repo: &Path,
    files: &[FileDiff],
    config: &ReviewConfig,
    rules: &Rules,
) -> Result<Packed, CliError> {
    let tracked = tracked_files(repo).map_err(|e| CliError::ReviewInput(e.to_string()))?;
    let extension = main_extension(tracked.iter().map(String::as_str));
    Ok(pack(
        files,
        PackOptions {
            budget: config.diff_budget_chars,
            ignore: &rules.ignore,
            main_extension: extension.as_deref(),
        },
    ))
}

/// What the reviewer said in one run.
struct Answer {
    text: String,
    model: Option<String>,
}

/// Something that can run the reviewer once.
trait Reviewer {
    /// Runs the reviewer. `hint` says what was wrong with the last answer, when there was one.
    async fn run(&mut self, hint: Option<String>) -> Result<Answer, CliError>;
}

/// The reviewer run by the daemon.
struct Live<'a, 'b> {
    ctx: &'a Context,
    io: &'a mut Io<'b>,
    force: bool,
    backend: BackendKind,
    cwd: &'a str,
    /// Where the reviewer reads its input, relative to the working directory.
    input: &'a str,
}

impl Reviewer for Live<'_, '_> {
    async fn run(&mut self, hint: Option<String>) -> Result<Answer, CliError> {
        let prompt = session_prompt(&task_prompt(self.input, hint.as_deref()));
        run_reviewer(
            self.ctx,
            self.io,
            self.force,
            self.backend,
            self.cwd,
            prompt,
        )
        .await
    }
}

/// Runs `reviewer` until its answer parses, at most [`MAX_ATTEMPTS`] times. Every run
/// after the first is given the hint that says what was wrong with the last answer.
async fn until_usable(
    packed: &Packed,
    max_findings: usize,
    reviewer: &mut impl Reviewer,
) -> Result<(Parsed, Option<String>), CliError> {
    let mut hint: Option<String> = None;
    let mut last = BadShape(String::new());
    for attempt in 1..=MAX_ATTEMPTS {
        info!(step = "review", attempt, "asking the reviewer");
        let answer = reviewer.run(hint.take()).await?;
        match parse_model_output(&answer.text, &packed.scope, max_findings) {
            Ok(parsed) => return Ok((parsed, answer.model)),
            Err(bad) => {
                hint = Some(bad.retry_hint());
                last = bad;
            }
        }
    }
    Err(CliError::ReviewOutput(last))
}

/// Runs one session that hands the work to the reviewer, and returns what the reviewer said.
async fn run_reviewer(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    backend: BackendKind,
    cwd: &str,
    prompt: String,
) -> Result<Answer, CliError> {
    let job = Job {
        prompt,
        cwd: cwd.to_owned(),
        backend: Some(backend),
        mode: Mode::Default,
        max_turns: None,
        token_cap: None,
    };
    let mut text: Option<String> = None;
    let mut watch = |kind: &str, body: &Value| {
        if let Some(reply) = reviewer_text(kind, body) {
            text = Some(reply);
        }
    };
    let mut tap = Tap {
        watch: &mut watch,
        print: false,
    };
    let done = drive(ctx, io, force, job, &mut tap).await?;
    let ended_well = !done.finish.failed
        && done
            .finish
            .condition
            .as_deref()
            .is_none_or(|condition| condition == END_TURN);
    if !ended_well {
        return Err(CliError::ReviewFailed(stop_reason(&done.finish)));
    }
    let Some(text) = text else {
        return Err(CliError::ReviewFailed(format!(
            "the {REVIEWER_AGENT} agent gave no answer; see the daemon log"
        )));
    };
    Ok(Answer {
        text,
        model: done.model,
    })
}

/// The text of a reply the reviewer agent wrote, when `kind` and `body` are that event.
fn reviewer_text(kind: &str, body: &Value) -> Option<String> {
    if kind != CHILD_EVENT_KIND || body["agent"] != REVIEWER_AGENT {
        return None;
    }
    if body["kind"] != ASSISTANT_MESSAGE_KIND {
        return None;
    }
    body["body"]["text"].as_str().map(str::to_owned)
}

/// The file the reviewer reads. It is removed when the review is over.
struct InputFile {
    dir: PathBuf,
    path: PathBuf,
    relative: String,
    created_parent: bool,
}

impl InputFile {
    fn write(cwd: &Path, text: &str) -> Result<Self, CliError> {
        let name = format!("input-{}.txt", std::process::id());
        let dir = cwd.join(INPUT_DIR);
        let created_parent = !cwd.join(INPUT_PARENT_DIR).exists();
        fs::create_dir_all(&dir).map_err(|source| CliError::Path {
            path: dir.clone(),
            source,
        })?;
        let path = dir.join(&name);
        let file = Self {
            relative: format!("{INPUT_DIR}/{name}"),
            dir,
            path,
            created_parent,
        };
        fs::write(&file.path, text).map_err(|source| CliError::Path {
            path: file.path.clone(),
            source,
        })?;
        Ok(file)
    }
}

impl Drop for InputFile {
    fn drop(&mut self) {
        // Nothing to report to: the file is a leftover at worst.
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.dir);
        if self.created_parent
            && let Some(parent) = self.dir.parent()
        {
            let _ = fs::remove_dir(parent);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::*;

    #[test]
    fn a_backend_that_runs_its_own_loop_is_refused_and_the_default_is_a_model_backend() {
        // Arrange
        let delegated = Config::from_toml_str("[providers.claude_code]\nenabled = true\n")
            .unwrap()
            .config;
        let faux = Config::from_toml_str("[providers.faux]\nenabled = true\n")
            .unwrap()
            .config;

        // Act and Assert
        assert!(matches!(
            review_backend(&delegated, Some(BackendKind::ClaudeCode)),
            Err(CliError::ReviewBackend { .. })
        ));
        assert_eq!(
            review_backend(&delegated, None).unwrap(),
            BackendKind::Anthropic
        );
        assert_eq!(review_backend(&faux, None).unwrap(), BackendKind::Faux);
    }

    #[test]
    fn only_the_reviewers_own_reply_is_the_answer() {
        // Arrange
        let reply = |agent: &str, kind: &str| serde_json::json!({"agent": agent, "kind": kind, "body": {"text": "x"}});

        // Act and Assert
        assert_eq!(
            reviewer_text(CHILD_EVENT_KIND, &reply("reviewer", "assistant_message")),
            Some("x".to_owned())
        );
        assert_eq!(
            reviewer_text(CHILD_EVENT_KIND, &reply("explore", "assistant_message")),
            None
        );
        assert_eq!(
            reviewer_text(CHILD_EVENT_KIND, &reply("reviewer", "tool_finished")),
            None
        );
        assert_eq!(
            reviewer_text("assistant_message", &serde_json::json!({"text": "x"})),
            None
        );
    }

    #[test]
    fn the_input_file_is_removed_with_the_directories_it_made() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();

        // Act
        let file = InputFile::write(dir.path(), "text").unwrap();
        let kept = fs::read_to_string(dir.path().join(&file.relative)).unwrap();
        drop(file);

        // Assert
        assert_eq!(kept, "text");
        assert!(!dir.path().join(INPUT_PARENT_DIR).exists());
    }

    #[test]
    fn an_existing_coda_directory_stays() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(INPUT_PARENT_DIR)).unwrap();

        // Act
        drop(InputFile::write(dir.path(), "text").unwrap());

        // Assert
        assert!(dir.path().join(INPUT_PARENT_DIR).exists());
        assert!(!dir.path().join(INPUT_DIR).exists());
    }

    fn packed() -> Packed {
        let mut scope = coda_review::schema::DiffScope::new();
        scope.add("a.txt", 1, 9);
        Packed {
            text: String::new(),
            scope,
            coverage: coda_review::schema::Coverage::default(),
            omitted: vec![],
        }
    }

    const GOOD: &str = r#"{"summary": "s", "effort": 1, "risk": "low", "findings": []}"#;

    /// A reviewer that gives its replies in turn and remembers the hints it was given.
    struct Scripted {
        replies: Vec<Result<&'static str, &'static str>>,
        hints: Vec<Option<String>>,
    }

    impl Reviewer for Scripted {
        fn run(&mut self, hint: Option<String>) -> impl Future<Output = Result<Answer, CliError>> {
            self.hints.push(hint);
            std::future::ready(match self.replies.remove(0) {
                Ok(text) => Ok(Answer {
                    text: text.to_owned(),
                    model: Some("m".to_owned()),
                }),
                Err(why) => Err(CliError::ReviewFailed(why.to_owned())),
            })
        }
    }

    fn scripted(replies: &[Result<&'static str, &'static str>]) -> Scripted {
        Scripted {
            replies: replies.to_vec(),
            hints: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_bad_shaped_answer_is_asked_for_again_once_with_a_hint() {
        // Arrange
        let mut reviewer = scripted(&[Ok("prose"), Ok(GOOD)]);

        // Act
        let (parsed, model) = until_usable(&packed(), 5, &mut reviewer).await.unwrap();

        // Assert
        assert_eq!(parsed.summary, "s");
        assert_eq!(model.as_deref(), Some("m"));
        assert_eq!(reviewer.hints.len(), 2);
        assert_eq!(reviewer.hints[0], None);
        assert!(
            reviewer.hints[1]
                .as_ref()
                .unwrap()
                .contains("Your last answer was rejected")
        );
    }

    #[tokio::test]
    async fn a_second_bad_shaped_answer_is_the_error_and_a_third_is_never_asked_for() {
        // Arrange
        let mut reviewer = scripted(&[Ok("prose"), Ok("more prose"), Ok(GOOD)]);

        // Act
        let result = until_usable(&packed(), 5, &mut reviewer).await;

        // Assert
        assert!(matches!(result, Err(CliError::ReviewOutput(_))));
        assert_eq!(reviewer.hints.len(), MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn a_failed_run_is_not_asked_again() {
        // Arrange
        let mut reviewer = scripted(&[Err("down"), Ok(GOOD)]);

        // Act
        let result = until_usable(&packed(), 5, &mut reviewer).await;

        // Assert
        assert!(matches!(result, Err(CliError::ReviewFailed(_))));
        assert_eq!(reviewer.hints.len(), 1);
    }
}
