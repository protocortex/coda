// SPDX-License-Identifier: Apache-2.0

//! `coda review`: packs a change, has the `reviewer` agent read it and prints the checked
//! findings. The command only reads: it posts nothing and edits no file of the repository.
//!
//! On Claude Code and Codex the daemon runs the reviewer on a session in plan mode, with the
//! packed diff in the prompt. On a model API the reviewer is a sub-agent of a session that
//! hands the work over through the `task` tool. The packed diff is too large to ask that
//! session's model to copy into a tool call, so it is written to a short-lived file in the
//! working directory, and the reviewer is told to read it.

use std::fs;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, ExitCode};
use std::task::Poll;

use coda_config::config::Config;
use coda_config::review::ReviewConfig;
use coda_core::history::{ASSISTANT_MESSAGE_KIND, CHILD_EVENT_KIND};
use coda_core::turn::StopCondition;
use coda_protocol::types::{BackendKind, Mode};
use coda_review::chunk::{Outcome, merge, plan};
use coda_review::diff::{FileDiff, git_diff, git_diff_staged, parse_diff, tracked_files};
use coda_review::pack::{PackOptions, Packed, main_extension};
use coda_review::parse::{BadShape, Parsed, answer_object, parse_model_output};
use coda_review::render::{to_json, to_markdown};
use coda_review::request::{
    GH_PR_FIELDS, Part, REVIEWER_AGENT, Role, Target, assemble, direct_prompt, input_text,
    pr_info_from_gh, session_prompt, task_prompt,
};
use coda_review::rules::{Rules, default_branch_ref, load};
use coda_review::schema::{Coverage, DiffScope, MIN_EFFORT, Review, Risk, RunDetails};
use coda_review::skip::Decision;
use serde_json::Value;
use tracing::info;

use crate::autostart::ensure_daemon;
use crate::cli::ReviewArgs;
use crate::context::{Context, Io};
use crate::daemon_cmd::load_config;
use crate::error::CliError;
use crate::run::{Job, STREAM_DELTA_KIND, Tap, drive, pick_backend, session_cwd, stop_reason};

/// Where the packed input is written, relative to the working directory.
const INPUT_DIR: &str = ".coda/review";
const INPUT_PARENT_DIR: &str = ".coda";
/// A review asks the reviewer again once when its answer has the wrong shape.
const MAX_ATTEMPTS: usize = 2;
/// A part's findings are not cut where they are read: the merge applies the cap, and it
/// must see every finding about security.
const NO_CAP: usize = usize::MAX;
const NOTHING_TO_REVIEW: &str = "nothing to review: the change has no lines to look at";
const NOTHING_LEFT: &str = "nothing to review: every changed file was left out by the review rules";
const FORCED_NOTE: &str = "a skip rule matched, but --force asks for the review anyway";
const GH: &str = "gh";
const HEAD: &str = "HEAD";
const END_TURN: &str = StopCondition::EndTurn.as_str();

/// Runs the review. `force` reviews a pull request that a skip rule would skip.
///
/// # Errors
/// Returns a [`CliError`] when the change cannot be read, the run fails, or the reviewer
/// does not answer in the review format twice.
pub async fn run(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    args: ReviewArgs,
) -> Result<ExitCode, CliError> {
    let config = load_config(&ctx.dirs)?;
    let backend = review_backend(&config, args.backend.map(Into::into));
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
                notice(io, args.json, &reason.one_line())?;
                return Ok(ExitCode::SUCCESS);
            }
            Decision::Forced(_) => writeln!(io.err, "coda: {FORCED_NOTE}")?,
            Decision::Review => {}
        }
    }
    let diff = change_text(repo, target.as_ref())?;
    let files = parse_diff(&diff);
    if files.is_empty() {
        notice(io, args.json, NOTHING_TO_REVIEW)?;
        return Ok(ExitCode::SUCCESS);
    }
    let parts = split_files(repo, &files, &config.review, &rules)?;
    if parts[0].coverage.reviewed.is_empty() && parts[0].coverage.clipped.is_empty() {
        notice(io, args.json, NOTHING_LEFT)?;
        return Ok(ExitCode::SUCCESS);
    }
    let inputs: Vec<String> = parts
        .iter()
        .enumerate()
        .map(|(n, part)| {
            input_text(
                part,
                args.instructions.as_deref(),
                config.review.max_findings,
                Part::of(n + 1, parts.len()),
            )
        })
        .collect();
    // Kept until the review is over, which removes the files.
    let written = if backend.is_model_backend() {
        Some(InputFiles::write(repo, &inputs)?)
    } else {
        None
    };
    // Every part runs on its own connection, so the daemon is started once, here, where
    // the user can be asked about a restart.
    drop(ensure_daemon(ctx, io, false).await?);
    let reviewers: Vec<Live<'_>> = (0..parts.len())
        .map(|n| Live {
            ctx,
            backend,
            cwd: &cwd,
            handoff: match &written {
                Some(files) => Handoff::File(&files.relative[n]),
                None => Handoff::Prompt(&inputs[n]),
            },
        })
        .collect();
    let results = review_parts(&parts, reviewers, config.review.chunk_concurrency).await;
    let given: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let input_chars = inputs.iter().map(|i| i.chars().count()).sum();
    let review = finish(
        &parts,
        results,
        &given,
        config.review.max_findings,
        (&backend_name(backend), input_chars),
    )?;
    if args.json {
        write!(io.out, "{}", to_json(&review)?)?;
    } else {
        write!(io.out, "{}", to_markdown(&review))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Tells the user why there is no review. With `--json` the notice is a review with no
/// findings, so what a script reads from standard output is always a review.
fn notice(io: &mut Io<'_>, json: bool, text: &str) -> Result<(), CliError> {
    if json {
        let empty = Review {
            summary: text.to_owned(),
            effort: MIN_EFFORT,
            risk: Risk::Low,
            findings: Vec::new(),
            coverage: Coverage::default(),
            run: RunDetails::default(),
        };
        write!(io.out, "{}", to_json(&empty)?)?;
    } else {
        writeln!(io.out, "{text}")?;
    }
    Ok(())
}

/// The backend that runs the review: the one asked for, else the one `coda run` would use.
fn review_backend(config: &Config, asked: Option<BackendKind>) -> BackendKind {
    asked.unwrap_or_else(|| pick_backend(config))
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

fn split_files(
    repo: &Path,
    files: &[FileDiff],
    config: &ReviewConfig,
    rules: &Rules,
) -> Result<Vec<Packed>, CliError> {
    let tracked = tracked_files(repo).map_err(|e| CliError::ReviewInput(e.to_string()))?;
    let extension = main_extension(tracked.iter().map(String::as_str));
    Ok(plan(
        files,
        PackOptions {
            budget: config.diff_budget_chars,
            ignore: &rules.ignore,
            main_extension: extension.as_deref(),
        },
        config.max_chunks,
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

/// How the input reaches the reviewer.
enum Handoff<'a> {
    /// A model API: a session hands the work to the reviewer, which reads this file,
    /// relative to the working directory.
    File(&'a str),
    /// Claude Code and Codex: the daemon runs the reviewer on this input as the prompt.
    Prompt(&'a str),
}

/// The reviewer run by the daemon.
struct Live<'a> {
    ctx: &'a Context,
    backend: BackendKind,
    cwd: &'a str,
    handoff: Handoff<'a>,
}

impl Reviewer for Live<'_> {
    async fn run(&mut self, hint: Option<String>) -> Result<Answer, CliError> {
        let hint = hint.as_deref();
        let request = match self.handoff {
            Handoff::File(path) => Request {
                prompt: session_prompt(Role::Reviewer, &task_prompt(Role::Reviewer, path, hint)),
                direct: false,
            },
            Handoff::Prompt(input) => Request {
                prompt: direct_prompt(Role::Reviewer, input, hint),
                direct: true,
            },
        };
        // Parts run side by side, so none of them writes to the terminal.
        let (mut out, mut err, mut input) = (io::sink(), io::sink(), io::empty());
        let mut quiet = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
        };
        run_reviewer(self.ctx, &mut quiet, self.backend, self.cwd, request).await
    }
}

/// One run of the reviewer.
struct Request {
    prompt: String,
    /// Whether the daemon runs the reviewer on the prompt itself, in plan mode, instead of
    /// a session handing the work over through the `task` tool.
    direct: bool,
}

/// Runs `reviewer` until its answer parses, at most [`MAX_ATTEMPTS`] times. Every run
/// after the first is given the hint that says what was wrong with the last answer.
async fn until_usable(
    scope: &DiffScope,
    reviewer: &mut impl Reviewer,
) -> Result<(Parsed, Option<String>), CliError> {
    let mut hint: Option<String> = None;
    let mut last = BadShape(String::new());
    for attempt in 1..=MAX_ATTEMPTS {
        info!(step = "review", attempt, "asking the reviewer");
        let answer = reviewer.run(hint.take()).await?;
        match parse_model_output(&answer.text, scope, NO_CAP) {
            Ok(parsed) => return Ok((parsed, answer.model)),
            Err(bad) => {
                hint = Some(bad.retry_hint());
                last = bad;
            }
        }
    }
    Err(CliError::ReviewOutput(last))
}

type PartResult = Result<(Parsed, Option<String>), CliError>;

/// Reviews one part. A run that fails for any reason but a bad answer is tried once more,
/// since a model or a connection can fail for a moment; an answer in the wrong shape has
/// already had its retry.
async fn review_part(scope: &DiffScope, reviewer: &mut impl Reviewer) -> PartResult {
    match until_usable(scope, reviewer).await {
        Err(CliError::ReviewOutput(bad)) => Err(CliError::ReviewOutput(bad)),
        Err(error) => {
            info!(step = "review", %error, "the run failed, trying the part once more");
            until_usable(scope, reviewer).await
        }
        done => done,
    }
}

/// Reviews every part, at most `concurrency` at a time. The results are in the order of
/// the parts.
async fn review_parts<R: Reviewer>(
    parts: &[Packed],
    mut reviewers: Vec<R>,
    concurrency: usize,
) -> Vec<PartResult> {
    let runs = parts
        .iter()
        .zip(reviewers.iter_mut())
        .map(|(part, reviewer)| review_part(&part.scope, reviewer))
        .collect();
    run_limited(runs, concurrency).await
}

/// Polls `futures` together, at most `limit` at a time: the next starts as soon as one
/// ends. Returns their outputs in the order of the futures.
async fn run_limited<F: Future>(futures: Vec<F>, limit: usize) -> Vec<F::Output> {
    let mut waiting = futures.into_iter().map(Box::pin).enumerate();
    let mut active: Vec<(usize, Pin<Box<F>>)> = Vec::new();
    let mut done: Vec<(usize, F::Output)> = Vec::new();
    std::future::poll_fn(|cx| {
        loop {
            while active.len() < limit.max(1) {
                match waiting.next() {
                    Some(next) => active.push(next),
                    None => break,
                }
            }
            if active.is_empty() {
                return Poll::Ready(());
            }
            let mut progressed = false;
            let mut still = Vec::with_capacity(active.len());
            for (index, mut future) in active.drain(..) {
                match future.as_mut().poll(cx) {
                    Poll::Ready(output) => {
                        done.push((index, output));
                        progressed = true;
                    }
                    Poll::Pending => still.push((index, future)),
                }
            }
            active = still;
            if !progressed {
                return Poll::Pending;
            }
        }
    })
    .await;
    done.sort_by_key(|(index, _)| *index);
    done.into_iter().map(|(_, output)| output).collect()
}

/// Builds the review from the results of the parts. `run` is the model name to give when no
/// part reported one, and the characters sent.
fn finish(
    parts: &[Packed],
    results: Vec<PartResult>,
    given: &[String],
    max_findings: usize,
    run: (&str, usize),
) -> Result<Review, CliError> {
    let (outcomes, model) = collect(parts, results)?;
    let merged = merge(given, outcomes, max_findings);
    Ok(assemble(
        merged,
        model.as_deref().unwrap_or(run.0),
        run.1,
        None,
    ))
}

/// Pairs each part with its result. The review fails only when every part did; otherwise a
/// failed part is an outcome with no answer, which makes the review partial.
fn collect(
    parts: &[Packed],
    results: Vec<PartResult>,
) -> Result<(Vec<Outcome<'_>>, Option<String>), CliError> {
    let mut outcomes = Vec::with_capacity(parts.len());
    let mut model = None;
    let mut last_error = None;
    for (part, result) in parts.iter().zip(results) {
        match result {
            Ok((parsed, used)) => {
                model = model.or(used);
                outcomes.push(Outcome {
                    packed: part,
                    parsed: Some(parsed),
                });
            }
            Err(error) => {
                info!(step = "review", %error, "a part could not be reviewed");
                last_error = Some(error);
                outcomes.push(Outcome {
                    packed: part,
                    parsed: None,
                });
            }
        }
    }
    match last_error {
        Some(error) if outcomes.iter().all(|o| o.parsed.is_none()) => Err(error),
        _ => Ok((outcomes, model)),
    }
}

/// Runs the reviewer in a new session and returns what it said. A direct run is the
/// reviewer on a session in plan mode, where every permission request is denied; its answer
/// is the text it streamed. Otherwise a session hands the work to the reviewer and the answer
/// is the reviewer's last reply.
async fn run_reviewer(
    ctx: &Context,
    io: &mut Io<'_>,
    backend: BackendKind,
    cwd: &str,
    request: Request,
) -> Result<Answer, CliError> {
    let Request { prompt, direct } = request;
    let job = Job {
        prompt,
        cwd: cwd.to_owned(),
        backend: Some(backend),
        agent: direct.then(|| REVIEWER_AGENT.to_owned()),
        mode: if direct { Mode::Plan } else { Mode::Default },
        max_turns: None,
        token_cap: None,
    };
    let mut reply: Option<String> = None;
    let mut streamed = String::new();
    let mut watch = |kind: &str, body: &Value| {
        if direct {
            if let Some(piece) = delta_text(kind, body) {
                streamed.push_str(piece);
            }
        } else if let Some(text) = reviewer_text(kind, body) {
            reply = Some(text);
        }
    };
    let mut tap = Tap {
        watch: &mut watch,
        print: false,
    };
    let done = drive(ctx, io, false, job, &mut tap).await?;
    let ended_well = !done.finish.failed
        && done
            .finish
            .condition
            .as_deref()
            .is_none_or(|condition| condition == END_TURN);
    if !ended_well {
        return Err(CliError::ReviewFailed(stop_reason(&done.finish)));
    }
    let text = if direct {
        Some(answer_object(&streamed).to_owned()).filter(|text| !text.trim().is_empty())
    } else {
        reply
    };
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

/// The text a streamed piece of a direct run holds, when `kind` and `body` are that event.
fn delta_text<'a>(kind: &str, body: &'a Value) -> Option<&'a str> {
    if kind != STREAM_DELTA_KIND {
        return None;
    }
    body["text"].as_str()
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

/// The files the reviewer reads, one per part. They are removed when the review is over.
struct InputFiles {
    dir: PathBuf,
    paths: Vec<PathBuf>,
    relative: Vec<String>,
    created_parent: bool,
}

impl InputFiles {
    fn write(cwd: &Path, texts: &[String]) -> Result<Self, CliError> {
        let dir = cwd.join(INPUT_DIR);
        let created_parent = !cwd.join(INPUT_PARENT_DIR).exists();
        let mut files = Self {
            dir: dir.clone(),
            paths: Vec::new(),
            relative: Vec::new(),
            created_parent,
        };
        fs::create_dir_all(&dir).map_err(|source| CliError::Path {
            path: dir.clone(),
            source,
        })?;
        for (n, text) in texts.iter().enumerate() {
            let name = input_name(n);
            let path = dir.join(&name);
            files.relative.push(format!("{INPUT_DIR}/{name}"));
            files.paths.push(path.clone());
            fs::write(&path, text).map_err(|source| CliError::Path { path, source })?;
        }
        Ok(files)
    }
}

/// The file of the part at `index`, from 0. The first keeps the name it always had.
fn input_name(index: usize) -> String {
    let pid = std::process::id();
    if index == 0 {
        format!("input-{pid}.txt")
    } else {
        format!("input-{pid}-{}.txt", index + 1)
    }
}

impl Drop for InputFiles {
    fn drop(&mut self) {
        // Nothing to report to: the files are a leftover at worst.
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
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
    fn the_backend_asked_for_is_used_and_the_default_is_the_one_coda_run_picks() {
        // Arrange
        let delegated = Config::from_toml_str("[providers.claude_code]\nenabled = true\n")
            .unwrap()
            .config;
        let faux = Config::from_toml_str("[providers.faux]\nenabled = true\n")
            .unwrap()
            .config;

        // Act and Assert
        assert_eq!(
            review_backend(&faux, Some(BackendKind::CodexAppServer)),
            BackendKind::CodexAppServer
        );
        assert_eq!(review_backend(&delegated, None), BackendKind::ClaudeCode);
        assert_eq!(review_backend(&faux, None), BackendKind::Faux);
    }

    #[test]
    fn only_the_stream_of_a_direct_run_is_its_answer() {
        // Arrange
        let delta = serde_json::json!({"text": "x"});

        // Act and Assert
        assert_eq!(delta_text(STREAM_DELTA_KIND, &delta), Some("x"));
        assert_eq!(delta_text("result", &delta), None);
        assert_eq!(delta_text(STREAM_DELTA_KIND, &serde_json::json!({})), None);
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
    fn the_input_files_are_removed_with_the_directories_they_made() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let texts = ["one".to_owned(), "two".to_owned()];

        // Act
        let files = InputFiles::write(dir.path(), &texts).unwrap();
        let kept: Vec<String> = files
            .relative
            .iter()
            .map(|path| fs::read_to_string(dir.path().join(path)).unwrap())
            .collect();
        drop(files);

        // Assert
        assert_eq!(kept, ["one", "two"]);
        assert!(!dir.path().join(INPUT_PARENT_DIR).exists());
    }

    #[test]
    fn the_first_input_file_keeps_its_name_and_the_others_are_numbered() {
        // Act and Assert
        let pid = std::process::id();
        assert_eq!(input_name(0), format!("input-{pid}.txt"));
        assert_eq!(input_name(1), format!("input-{pid}-2.txt"));
    }

    #[test]
    fn an_existing_coda_directory_stays() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(INPUT_PARENT_DIR)).unwrap();

        // Act
        drop(InputFiles::write(dir.path(), &["text".to_owned()]).unwrap());

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
        let (parsed, model) = until_usable(&packed().scope, &mut reviewer).await.unwrap();

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
        let result = until_usable(&packed().scope, &mut reviewer).await;

        // Assert
        assert!(matches!(result, Err(CliError::ReviewOutput(_))));
        assert_eq!(reviewer.hints.len(), MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn a_failed_run_is_not_asked_again() {
        // Arrange
        let mut reviewer = scripted(&[Err("down"), Ok(GOOD)]);

        // Act
        let result = until_usable(&packed().scope, &mut reviewer).await;

        // Assert
        assert!(matches!(result, Err(CliError::ReviewFailed(_))));
        assert_eq!(reviewer.hints.len(), 1);
    }

    fn two_parts() -> Vec<Packed> {
        let part = |file: &str| {
            let mut part = packed();
            part.coverage.reviewed = vec![file.to_owned()];
            part
        };
        vec![part("a.txt"), part("b.txt")]
    }

    #[tokio::test]
    async fn a_run_that_fails_is_tried_once_more_and_only_once() {
        // Arrange
        let mut again = scripted(&[Err("down"), Ok(GOOD)]);
        let mut twice = scripted(&[Err("down"), Err("down"), Ok(GOOD)]);

        // Act
        let recovered = review_part(&packed().scope, &mut again).await;
        let failed = review_part(&packed().scope, &mut twice).await;

        // Assert
        assert!(recovered.is_ok());
        assert!(matches!(failed, Err(CliError::ReviewFailed(_))));
        assert_eq!(twice.hints.len(), 2);
    }

    #[tokio::test]
    async fn an_answer_of_the_wrong_shape_twice_is_not_tried_a_third_time() {
        // Arrange
        let mut reviewer = scripted(&[Ok("prose"), Ok("more"), Ok(GOOD)]);

        // Act
        let result = review_part(&packed().scope, &mut reviewer).await;

        // Assert
        assert!(matches!(result, Err(CliError::ReviewOutput(_))));
        assert_eq!(reviewer.hints.len(), MAX_ATTEMPTS);
    }

    #[tokio::test]
    async fn a_failed_part_is_an_outcome_without_an_answer_and_all_failing_is_an_error() {
        // Arrange
        let parts = two_parts();
        let some = vec![
            Ok((parsed_of(GOOD), Some("m".to_owned()))),
            Err(CliError::ReviewFailed("down".to_owned())),
        ];
        let none = vec![
            Err(CliError::ReviewFailed("first".to_owned())),
            Err(CliError::ReviewFailed("second".to_owned())),
        ];

        // Act
        let (outcomes, model) = collect(&parts, some).unwrap();
        let error = collect(&parts, none).err().unwrap();

        // Assert
        assert!(outcomes[0].parsed.is_some());
        assert!(outcomes[1].parsed.is_none());
        assert_eq!(model.as_deref(), Some("m"));
        assert!(matches!(error, CliError::ReviewFailed(why) if why == "second"));
    }

    fn parsed_of(text: &str) -> Parsed {
        parse_model_output(text, &packed().scope, NO_CAP).unwrap()
    }

    /// A reviewer that records how many runs are in flight and waits a few turns of the
    /// executor, so runs that are polled together overlap.
    struct Counting {
        running: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        most: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Reviewer for Counting {
        async fn run(&mut self, _hint: Option<String>) -> Result<Answer, CliError> {
            use std::sync::atomic::Ordering;
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            self.running.fetch_sub(1, Ordering::SeqCst);
            Ok(Answer {
                text: GOOD.to_owned(),
                model: None,
            })
        }
    }

    #[tokio::test]
    async fn parts_run_together_up_to_the_limit_and_the_results_keep_their_order() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        for (limit, expected_most) in [(1, 1), (2, 2), (9, 5)] {
            // Arrange
            let running = Arc::new(AtomicUsize::new(0));
            let most = Arc::new(AtomicUsize::new(0));
            let parts: Vec<Packed> = (0..5).map(|_| packed()).collect();
            let reviewers = (0..5)
                .map(|_| Counting {
                    running: Arc::clone(&running),
                    most: Arc::clone(&most),
                })
                .collect();

            // Act
            let results = review_parts(&parts, reviewers, limit).await;

            // Assert
            assert_eq!(results.len(), 5);
            assert!(results.iter().all(Result::is_ok));
            assert_eq!(most.load(Ordering::SeqCst), expected_most, "limit {limit}");
        }
    }

    #[tokio::test]
    async fn results_are_in_the_order_of_the_parts_whatever_finishes_first() {
        // Arrange
        let parts = two_parts();
        let reviewers = vec![
            scripted(&[Err("first"), Err("first")]),
            scripted(&[Ok(GOOD)]),
        ];

        // Act
        let results = review_parts(&parts, reviewers, 2).await;

        // Assert
        assert!(matches!(&results[0], Err(CliError::ReviewFailed(why)) if why == "first"));
        assert!(results[1].is_ok());
    }

    #[test]
    fn a_review_with_one_failed_part_is_partial_keeps_the_other_findings_and_shows_the_banner() {
        // Arrange
        let parts = two_parts();
        let finding = r#"{"file": "a.txt", "start_line": 3, "end_line": 3, "severity": "high",
            "category": "bug", "title": "T", "explanation": "E", "confidence": "high"}"#;
        let answer = format!(
            r#"{{"summary": "s", "effort": 2, "risk": "medium", "findings": [{finding}]}}"#
        );
        let results = vec![
            Ok((parsed_of(&answer), None)),
            Err(CliError::ReviewFailed("down".to_owned())),
        ];
        let given = vec!["a.txt".to_owned(), "b.txt".to_owned()];

        // Act
        let review = finish(&parts, results, &given, 5, ("claude_code", 10)).unwrap();
        let text = to_markdown(&review);

        // Assert
        assert!(review.run.partial);
        assert_eq!(review.run.chunks, 2);
        assert_eq!(review.run.model, "claude_code");
        assert_eq!(review.findings.len(), 1);
        assert_eq!(review.coverage.reviewed, ["a.txt"]);
        assert_eq!(review.coverage.left_out, ["b.txt"]);
        assert!(
            text.starts_with("# Review\n\n**Partial review:**"),
            "{text}"
        );
    }
}
