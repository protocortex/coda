// SPDX-License-Identifier: Apache-2.0

//! `coda review`: packs a change, has the `reviewer` agent read it and prints the checked
//! findings. The command only reads, unless `--post` asks it to put the findings on a pull
//! request as a pending review; it edits no file of the repository.
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
use std::sync::Mutex;
use std::task::Poll;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use coda_config::config::Config;
use coda_config::review::ReviewConfig;
use coda_core::history::{ASSISTANT_MESSAGE_KIND, CHILD_EVENT_KIND};
use coda_core::turn::StopCondition;
use coda_protocol::events::{USAGE_KIND, USAGE_LIMIT_KIND};
use coda_protocol::types::{BackendKind, Mode};
use coda_review::chunk::{Merged, Outcome, merge, plan};
use coda_review::context::{ContextFile, prompt_section as context_section};
use coda_review::diff::{FileDiff, git_diff, git_diff_staged, parse_diff, tracked_files};
use coda_review::guidance;
use coda_review::issues::{check_compliance, prompt_section};
use coda_review::memory::Scope;
use coda_review::pack::{PackOptions, Packed, main_extension};
use coda_review::parse::{BadShape, Parsed, parse_model_output};
use coda_review::render::{to_json, to_markdown};
use coda_review::report::{RunFacts, RunReport, Tokens, append};
use coda_review::request::{
    Extras, GH_PR_FIELDS, Part, Role, Target, VerifierRun, assemble, direct_prompt, input_text,
    pr_info_from_gh, session_prompt, task_prompt, tester_task,
};
use coda_review::rules::{Rules, default_branch_ref, load};
use coda_review::schema::{Coverage, DiffScope, Finding, MIN_EFFORT, Review, Risk, RunDetails};
use coda_review::skip::Decision;
use coda_review::verify::{self, Applied, parse_verdicts};
use serde_json::Value;
use tracing::info;

use crate::autostart::ensure_daemon;
use crate::cli::ReviewArgs;
use crate::context::{Context, Io};
use crate::daemon_cmd::load_config;
use crate::error::{CliError, Unavailable};
use crate::exit;
use crate::review_context::{Asked, run_checks};
use crate::review_issues::{Linked, gather as gather_issues};
use crate::review_memory::{Memory, saw_everything};
use crate::review_post::{Confirm, RealGh, head_of, post};
use crate::review_route::{Attempt, Failover, other_backend, unavailable_reason};
use crate::run::{
    CANCELLED, Finish, Job, STREAM_DELTA_KIND, Tap, drive, pick_backend, session_cwd, stop_reason,
};

/// Where the packed input is written, relative to the working directory.
const INPUT_DIR: &str = ".coda/review";
const INPUT_PARENT_DIR: &str = ".coda";
/// Names of the files of the reviewer's and of the verifier's input.
const INPUT_FILE_PREFIX: &str = "input";
const VERIFY_FILE_PREFIX: &str = "verify";
/// A review asks the reviewer again once when its answer has the wrong shape.
const MAX_ATTEMPTS: usize = 2;
/// A part's findings are not cut where they are read: the merge applies the cap, and it
/// must see every finding about security.
const NO_CAP: usize = usize::MAX;
const NOTHING_TO_REVIEW: &str = "nothing to review: the change has no lines to look at";
const NOTHING_LEFT: &str = "nothing to review: every changed file was left out by the review rules";
const NO_DEFAULT_BRANCH_NOTE: &str =
    "the repository has no default branch to read project rules from, so none are used";
const FORCED_NOTE: &str = "a skip rule matched, but --force asks for the review anyway";
const VERIFIER_LABEL: &str = "verifier";
const CHECKS_NOT_RUN_NOTE: &str =
    "the checks did not run, so the review has no check output; the reviewer is told so";
const GH: &str = "gh";
const HEAD: &str = "HEAD";
/// The branch a push goes to.
const UPSTREAM: &str = "@{u}";
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
    if args.hook {
        return run_hook(ctx, io, force, args).await;
    }
    run_review(ctx, io, force, args).await
}

/// What the pre-push hook runs. It reviews nothing unless the global config turns it on, and
/// a branch with no upstream or a review that fails does not stop the push: only a finding
/// of severity high or critical can, when the config says so.
async fn run_hook(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    args: ReviewArgs,
) -> Result<ExitCode, CliError> {
    match hook_review(ctx, io, force, args).await {
        Err(error) => {
            writeln!(
                io.err,
                "coda: the review before the push failed, so the push goes on: {error}"
            )?;
            Ok(ExitCode::SUCCESS)
        }
        done => done,
    }
}

/// What is reviewed: nothing named for `--staged`, the commits about to be pushed for the
/// hook, else what the user typed.
fn review_target(args: &ReviewArgs) -> Option<Target> {
    if args.staged {
        return None;
    }
    if args.hook {
        return Some(Target::Range {
            base: UPSTREAM.to_owned(),
            head: HEAD.to_owned(),
        });
    }
    Some(Target::parse(args.target.as_deref()))
}

/// Whether the hook run stops the push: the config says so and a finding of severity high or
/// critical remains.
fn blocks_push(args: &ReviewArgs, config: &ReviewConfig, review: &Review) -> bool {
    args.hook && config.hook.block_on_high && review.findings.iter().any(|f| f.severity.blocks())
}

async fn hook_review(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    args: ReviewArgs,
) -> Result<ExitCode, CliError> {
    if !load_config(&ctx.dirs)?.review.hook.enabled {
        return Ok(ExitCode::SUCCESS);
    }
    let cwd = session_cwd(&ctx.cwd, args.cwd.clone())?;
    if !has_upstream(Path::new(&cwd)) {
        writeln!(
            io.err,
            "coda: the branch has no upstream, so there is nothing to review before the push"
        )?;
        return Ok(ExitCode::SUCCESS);
    }
    run_review(ctx, io, force, args).await
}

fn has_upstream(repo: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", UPSTREAM])
        .current_dir(repo)
        .output()
        .is_ok_and(|out| out.status.success())
}

async fn run_review(
    ctx: &Context,
    io: &mut Io<'_>,
    force: bool,
    args: ReviewArgs,
) -> Result<ExitCode, CliError> {
    let started = Instant::now();
    let config = load_config(&ctx.dirs)?;
    let backend = review_backend(&config, args.backend.map(Into::into));
    let cwd = session_cwd(&ctx.cwd, args.cwd.clone())?;
    let repo = Path::new(&cwd);
    info!(step = "read_change", "reading the change");
    let rules = load_rules(&config.review, repo)?;
    let asked = Asked::new(&args, &config.review, backend)?;
    let target = review_target(&args);
    let post_to = post_request(ctx, repo, &args, target.as_ref())?;
    if !args.dismiss.is_empty() {
        return dismiss_only(ctx, io, repo, &args, target.as_ref());
    }
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
    let (mut memory, scope) = open_memory(ctx, io, repo, &args, target.as_ref(), &config.review)?;
    if let Scope::Skip(why) = &scope {
        notice(io, args.json, why)?;
        return Ok(ExitCode::SUCCESS);
    }
    let (diff, whole) = read_change(io, repo, target.as_ref(), memory.as_ref(), &scope)?;
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
    let tally = TokenTally::default();
    let place = Placement {
        ctx,
        config: &config,
        cwd: &cwd,
        backend,
        tally: &tally,
    };
    let gathered = gather_context(io, &place, &asked, args.context_note.as_deref()).await?;
    let sources = Sources {
        repo,
        config: &config,
        rules: &rules,
        args: &args,
        target: target.as_ref(),
        memory: memory.as_ref(),
        context: &gathered.section,
    };
    let (inputs, linked) = sources.inputs(io, &parts)?;
    // Kept until the review is over, which removes the files.
    let written = if backend.is_model_backend() {
        Some(InputFiles::write(repo, INPUT_FILE_PREFIX, &inputs)?)
    } else {
        None
    };
    // Every part runs on its own connection, so the daemon is started once, here, where
    // the user can be asked about a restart.
    drop(ensure_daemon(ctx, io, false).await?);
    let mut reviewers = part_reviewers(&place, &parts, &inputs, written.as_ref());
    let results = review_parts(&parts, &mut reviewers, config.review.chunk_concurrency).await;
    let fallbacks: Vec<String> = reviewers
        .iter()
        .flat_map(|reviewer| reviewer.notes().to_vec())
        .collect();
    let given: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let input_chars = inputs.iter().map(|i| i.chars().count()).sum();
    let (mut merged, model) = merged_review(&parts, results, &given, config.review.max_findings)?;
    if let Some(m) = &memory {
        m.drop_dismissed(&mut merged.parsed);
    }
    let verified = verify_if_asked(&place, repo, &parts, &mut merged).await?;
    let model = model.unwrap_or_else(|| backend_name(backend));
    let mut review = assemble_linked(merged, (&model, input_chars, verified), linked);
    review.run.fallbacks = fallbacks;
    print_review(io, args.json, &review)?;
    report_run(io, &place, &review, (started, &gathered))?;
    if let Some(m) = memory.as_mut() {
        remember(io, m, &review, &files, saw_everything(&review, whole))?;
    }
    if let Some(to) = post_to {
        post_review(io, repo, to, &parts, &review)?;
    }
    if blocks_push(&args, &config.review, &review) {
        return Ok(ExitCode::from(exit::REVIEW_BLOCKED));
    }
    Ok(ExitCode::SUCCESS)
}

/// Where the parts of a review run.
struct Placement<'a> {
    ctx: &'a Context,
    config: &'a Config,
    cwd: &'a str,
    backend: BackendKind,
    tally: &'a TokenTally,
}

/// What the person added to the prompt, ready to go in it.
struct Gathered {
    section: String,
    files: usize,
    checks_run: bool,
}

/// Makes the context section of the prompt from the context files and, when asked, from the
/// output of the checks, which the `tester` agent runs here.
async fn gather_context(
    io: &mut Io<'_>,
    place: &Placement<'_>,
    asked: &Asked,
    note: Option<&str>,
) -> Result<Gathered, CliError> {
    let mut files: Vec<ContextFile> = asked.files.clone();
    let mut checks_run = false;
    if let Some(commands) = &asked.commands {
        drop(ensure_daemon(place.ctx, io, false).await?);
        let task = tester_task(commands);
        let mut tester = Live {
            ctx: place.ctx,
            role: Role::Tester,
            cwd: place.cwd,
            handoff: Handoff::Task(&task),
            tally: place.tally,
        };
        let checks = run_checks(&mut tester, place.backend).await?;
        if !checks.ran {
            writeln!(io.err, "coda: {CHECKS_NOT_RUN_NOTE}")?;
        }
        checks_run = checks.ran;
        files.push(checks.file);
    }
    Ok(Gathered {
        section: context_section(&files, note),
        files: files.len(),
        checks_run,
    })
}

/// Runs the verifier on the findings of `merged` when the config has it on and there are
/// any.
async fn verify_if_asked(
    place: &Placement<'_>,
    repo: &Path,
    parts: &[Packed],
    merged: &mut Merged,
) -> Result<Option<VerifierRun>, CliError> {
    if !place.config.review.verify || merged.parsed.findings.is_empty() {
        return Ok(None);
    }
    let at = Checking {
        ctx: place.ctx,
        repo,
        cwd: place.cwd,
        config: place.config,
        reviewed_on: place.backend,
        tally: place.tally,
    };
    Ok(Some(check_findings(&at, parts, merged).await?))
}

/// Prints the report of the run on standard error and adds it to the report file in the data
/// directory. A file that cannot be written is a note: the review is already out.
fn report_run(
    io: &mut Io<'_>,
    place: &Placement<'_>,
    review: &Review,
    (started, gathered): (Instant, &Gathered),
) -> Result<(), CliError> {
    let facts = RunFacts {
        at: now_secs(),
        backend: backend_name(place.backend),
        wall_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        context_files: gathered.files,
        checks_run: gathered.checks_run,
        tokens: place.tally.total(),
    };
    let report = RunReport::of(review, facts);
    write!(io.err, "{}", report.text())?;
    if let Err(error) = append(&place.ctx.dirs.data_dir(), &report) {
        writeln!(
            io.err,
            "coda: could not add the run to the report file: {error}"
        )?;
    }
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// One reviewer for each part: on the backend asked for, set to the model the routing rules
/// pick for the part's size, and with the other backend behind it when fallback is on.
fn part_reviewers<'a>(
    place: &Placement<'a>,
    parts: &[Packed],
    inputs: &'a [String],
    written: Option<&'a InputFiles>,
) -> Vec<Failover<Live<'a>>> {
    let routing = &place.config.review.routing;
    let mut order = vec![place.backend];
    if routing.fallback {
        order.extend(other_backend(place.config, place.backend));
    }
    parts
        .iter()
        .enumerate()
        .map(|(n, part)| {
            let live = Live {
                ctx: place.ctx,
                role: Role::Reviewer,
                cwd: place.cwd,
                handoff: match written {
                    Some(files) => Handoff::File(&files.relative[n]),
                    None => Handoff::Prompt(&inputs[n]),
                },
                tally: place.tally,
            };
            let model = routing.model_for(part.size()).map(str::to_owned);
            let label = format!("part {} of {}", n + 1, parts.len());
            Failover::new(live, order.clone(), model, label)
        })
        .collect()
}

/// The review memory of the pull request, when there is one, and what this run reviews.
fn open_memory(
    ctx: &Context,
    io: &mut Io<'_>,
    repo: &Path,
    args: &ReviewArgs,
    target: Option<&Target>,
    config: &ReviewConfig,
) -> Result<(Option<Memory>, Scope), CliError> {
    let memory = Memory::open(ctx, repo, target, io, args.incremental)?;
    let scope = memory
        .as_ref()
        .map_or(Scope::Full, |m| m.scope(args.incremental, repo, config));
    Ok((memory, scope))
}

/// Everything the prompts are built from.
struct Sources<'a> {
    repo: &'a Path,
    config: &'a Config,
    rules: &'a Rules,
    args: &'a ReviewArgs,
    target: Option<&'a Target>,
    memory: Option<&'a Memory>,
    context: &'a str,
}

impl Sources<'_> {
    /// The reviewer's input for each part, and the issues the pull request links to.
    fn inputs(&self, io: &mut Io<'_>, parts: &[Packed]) -> Result<(Vec<String>, Linked), CliError> {
        let (repo, review) = (self.repo, &self.config.review);
        note_missing_default_branch(io, review, repo)?;
        let rules_text = read_guidance(review, repo)?;
        let linked = linked_issues(repo, self.target, review);
        let issues_text = prompt_section(&linked.issues);
        let earlier = self.memory.map(Memory::prompt).unwrap_or_default();
        let extras = Extras {
            guidance: Some(&rules_text),
            standing: Some(&self.rules.extra_instructions),
            run: self.args.instructions.as_deref(),
            issues: Some(&issues_text),
            earlier: Some(&earlier),
            context: Some(self.context),
        };
        let inputs = parts
            .iter()
            .enumerate()
            .map(|(n, part)| {
                let at = Part::of(n + 1, parts.len());
                input_text(part, &extras, review.max_findings, at)
            })
            .collect();
        Ok((inputs, linked))
    }
}

/// The diff to review. With an incremental scope it is what came after the last run; if git
/// cannot give that, such as after a force push, it is the whole change, said on standard
/// error. The flag says whether the whole change was read.
fn read_change(
    io: &mut Io<'_>,
    repo: &Path,
    target: Option<&Target>,
    memory: Option<&Memory>,
    scope: &Scope,
) -> Result<(String, bool), CliError> {
    if let (Scope::Since(sha), Some(memory)) = (scope, memory) {
        match memory.diff_since(repo, sha) {
            Ok(diff) => return Ok((diff, false)),
            Err(error) => writeln!(io.err, "coda: reviewing everything instead: {error}")?,
        }
    }
    Ok((change_text(repo, target)?, true))
}

/// Records the run in the review memory. A failure is a note: the review is already out.
fn remember(
    io: &mut Io<'_>,
    memory: &mut Memory,
    review: &Review,
    files: &[FileDiff],
    sees_everything: bool,
) -> Result<(), CliError> {
    if let Err(error) = memory.record(&review.findings, files, sees_everything) {
        writeln!(io.err, "coda: could not remember this review: {error}")?;
    }
    Ok(())
}

/// `--dismiss`: marks findings as dismissed in the memory of the pull request and stops.
fn dismiss_only(
    ctx: &Context,
    io: &mut Io<'_>,
    repo: &Path,
    args: &ReviewArgs,
    target: Option<&Target>,
) -> Result<ExitCode, CliError> {
    let Some(mut memory) = Memory::open(ctx, repo, target, io, true)? else {
        return Err(CliError::ReviewMemory("no review memory".to_owned()));
    };
    memory.dismiss(&args.dismiss, io)?;
    Ok(ExitCode::SUCCESS)
}

/// The issues a pull request links to. A branch, range or staged change has none.
fn linked_issues(repo: &Path, target: Option<&Target>, config: &ReviewConfig) -> Linked {
    match target {
        Some(Target::PullRequest(pr)) => {
            gather_issues(&mut RealGh { repo }, pr, &config.linked_repos)
        }
        _ => Linked::default(),
    }
}

/// Builds the review. The compliance verdicts the issues do not back are dropped, so with no
/// issue read there are none, and the notes of the skipped issues go in the run details.
fn assemble_linked(
    mut merged: Merged,
    (model, input_chars, verified): (&str, usize, Option<VerifierRun>),
    linked: Linked,
) -> Review {
    let given = std::mem::take(&mut merged.parsed.compliance);
    let (kept, dropped) = check_compliance(given, &linked.issues);
    if dropped > 0 {
        info!(
            step = "compliance",
            dropped, "dropped verdicts the issues do not back"
        );
    }
    merged.parsed.compliance = kept;
    let mut review = assemble(merged, model, input_chars, verified);
    review.run.skipped_issues = linked.notes;
    review
}

fn print_review(io: &mut Io<'_>, json: bool, review: &Review) -> Result<(), CliError> {
    if json {
        write!(io.out, "{}", to_json(review)?)?;
    } else {
        write!(io.out, "{}", to_markdown(review))?;
    }
    Ok(())
}

fn post_review(
    io: &mut Io<'_>,
    repo: &Path,
    to: PostTo,
    parts: &[Packed],
    review: &Review,
) -> Result<(), CliError> {
    let scopes: Vec<&DiffScope> = parts.iter().map(|part| &part.scope).collect();
    let PostTo { pr, confirm, head } = to;
    post(
        &mut RealGh { repo },
        io,
        confirm,
        &pr,
        &head,
        review,
        &scopes,
    )
}

/// Where `--post` posts, how it is confirmed, and the commit the review is of.
struct PostTo {
    pr: String,
    confirm: Confirm,
    head: String,
}

/// What `--post` posts to and how it is confirmed, checked before any model run is paid for.
/// `None` without `--post`.
fn post_request(
    ctx: &Context,
    repo: &Path,
    args: &ReviewArgs,
    target: Option<&Target>,
) -> Result<Option<PostTo>, CliError> {
    if !args.post {
        return Ok(None);
    }
    let confirm = Confirm {
        assume_yes: args.yes,
        interactive: ctx.stdin_is_tty,
    };
    confirm.check()?;
    match target {
        Some(Target::PullRequest(pr)) => Ok(Some(PostTo {
            head: head_of(&mut RealGh { repo }, pr)?,
            pr: pr.clone(),
            confirm,
        })),
        _ => Err(CliError::ReviewPost(
            "only a pull request can be posted to; name one by number or URL".to_owned(),
        )),
    }
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
            compliance: Vec::new(),
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
pub(crate) fn backend_name(backend: BackendKind) -> String {
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

/// Says on standard error that no project rules can be read, when they are wanted.
fn note_missing_default_branch(
    io: &mut Io<'_>,
    config: &ReviewConfig,
    repo: &Path,
) -> Result<(), CliError> {
    if config.guidance_lines > 0 && default_branch_ref(repo).is_none() {
        writeln!(io.err, "coda: {NO_DEFAULT_BRANCH_NOTE}")?;
    }
    Ok(())
}

/// The project rules of the repository, read from its default branch only. Empty when the
/// cap is zero or the repository has no default branch to read them from.
fn read_guidance(config: &ReviewConfig, repo: &Path) -> Result<String, CliError> {
    let Some(default_ref) = default_branch_ref(repo) else {
        return Ok(String::new());
    };
    guidance::load(repo, &default_ref, config.guidance_lines)
        .map(|found| found.text)
        .map_err(|e| CliError::ReviewInput(e.to_string()))
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
#[derive(Debug)]
pub(crate) struct Answer {
    pub(crate) text: String,
    pub(crate) model: Option<String>,
}

/// Something that can run the reviewer once.
pub(crate) trait Reviewer {
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
    /// A model API: a session hands this short text to the agent through the `task` tool,
    /// with the session open to the user's standing grants.
    Task(&'a str),
}

/// The reviewer run by the daemon.
struct Live<'a> {
    ctx: &'a Context,
    role: Role,
    cwd: &'a str,
    handoff: Handoff<'a>,
    tally: &'a TokenTally,
}

/// The tokens the backends reported over a whole review, when any did.
#[derive(Debug, Default)]
struct TokenTally(Mutex<Option<Tokens>>);

impl TokenTally {
    fn add(&self, input: u64, output: u64) {
        if let Ok(mut total) = self.0.lock() {
            let sum = total.get_or_insert(Tokens {
                input: 0,
                output: 0,
            });
            sum.input = sum.input.saturating_add(input);
            sum.output = sum.output.saturating_add(output);
        }
    }

    fn total(&self) -> Option<Tokens> {
        self.0.lock().ok().and_then(|total| *total)
    }
}

/// The input and output tokens a `usage` event reports, when it reports any.
fn usage_tokens(kind: &str, body: &Value) -> Option<(u64, u64)> {
    if kind != USAGE_KIND {
        return None;
    }
    let usage = &body["usage"];
    let input = usage["input_tokens"].as_u64().unwrap_or(0);
    let output = usage["output_tokens"].as_u64().unwrap_or(0);
    (input.saturating_add(output) > 0).then_some((input, output))
}

impl Attempt for Live<'_> {
    async fn attempt(
        &mut self,
        backend: BackendKind,
        model: Option<&str>,
        hint: Option<String>,
    ) -> Result<Answer, CliError> {
        let hint = hint.as_deref();
        let request = match self.handoff {
            Handoff::File(path) => Request {
                prompt: session_prompt(self.role, &task_prompt(self.role, path, hint)),
                role: self.role,
                direct: false,
            },
            Handoff::Prompt(input) => Request {
                prompt: direct_prompt(self.role, input, hint),
                role: self.role,
                direct: true,
            },
            Handoff::Task(text) => Request {
                prompt: session_prompt(self.role, text),
                role: self.role,
                direct: false,
            },
        };
        // Parts run side by side, so none of them writes to the terminal.
        let (mut out, mut err, mut input) = (io::sink(), io::sink(), io::empty());
        let mut quiet = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
        };
        let on = Backend {
            kind: backend,
            model,
        };
        run_reviewer(self.ctx, &mut quiet, on, self.cwd, request, self.tally).await
    }
}

/// The backend an agent runs on, and the model it is set to when there is one. Claude Code
/// and Codex start on their own default model whatever is set here.
struct Backend<'a> {
    kind: BackendKind,
    model: Option<&'a str>,
}

/// One run of an agent.
struct Request {
    prompt: String,
    role: Role,
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

/// Reviews one part. A run that fails for any reason but a bad answer, an interrupt or a
/// usage limit is tried once more, since a model or a connection can fail for a moment; an
/// answer in the wrong shape has already had its retry, and a usage limit does not pass.
async fn review_part(scope: &DiffScope, reviewer: &mut impl Reviewer) -> PartResult {
    match until_usable(scope, reviewer).await {
        Err(
            error @ (CliError::ReviewOutput(_)
            | CliError::RunInterrupted
            | CliError::BackendUnavailable(Unavailable::UsageLimit)),
        ) => Err(error),
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
    reviewers: &mut [R],
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

/// Merges the results of the parts. Also returns the model that wrote the first answer.
fn merged_review(
    parts: &[Packed],
    results: Vec<PartResult>,
    given: &[String],
    max_findings: usize,
) -> Result<(Merged, Option<String>), CliError> {
    let (outcomes, model) = collect(parts, results)?;
    Ok((merge(given, outcomes, max_findings), model))
}

/// Where and how the findings of a review are checked.
struct Checking<'a> {
    ctx: &'a Context,
    repo: &'a Path,
    cwd: &'a str,
    config: &'a Config,
    reviewed_on: BackendKind,
    tally: &'a TokenTally,
}

/// Has the verifier check the findings of `merged` against the diff of `parts`, and keeps
/// what it lets through.
async fn check_findings(
    at: &Checking<'_>,
    parts: &[Packed],
    merged: &mut Merged,
) -> Result<VerifierRun, CliError> {
    let backend = verifier_backend(at.config, at.reviewed_on);
    let diffs: Vec<&str> = parts.iter().map(|p| p.text.as_str()).collect();
    let input = verify::input_text(&merged.parsed.findings, &diffs);
    let file = if backend.is_model_backend() {
        match InputFiles::write(at.repo, VERIFY_FILE_PREFIX, std::slice::from_ref(&input)) {
            Ok(file) => Some(file),
            Err(error) => {
                info!(step = "verify", %error, "the verifier input could not be written");
                let findings = std::mem::take(&mut merged.parsed.findings);
                merged.parsed.findings = verify::apply(findings, None, 0).findings;
                return Ok(VerifierRun {
                    backend: backend_name(backend),
                    dropped: 0,
                });
            }
        }
    } else {
        None
    };
    let live = Live {
        ctx: at.ctx,
        role: Role::Verifier,
        cwd: at.cwd,
        handoff: match &file {
            Some(file) => Handoff::File(&file.relative[0]),
            None => Handoff::Prompt(&input),
        },
        tally: at.tally,
    };
    let mut live = Failover::new(live, vec![backend], None, VERIFIER_LABEL.to_owned());
    let findings = std::mem::take(&mut merged.parsed.findings);
    let Applied { findings, dropped } =
        verify_findings(findings, at.config.review.verify_threshold, &mut live).await?;
    merged.parsed.findings = findings;
    Ok(VerifierRun {
        backend: backend_name(backend),
        dropped,
    })
}

/// The backend that checks the findings: the other of Claude Code and Codex when both are
/// enabled, so the second look does not come from the model that wrote the first. Otherwise
/// the backend of the review.
fn verifier_backend(config: &Config, reviewer: BackendKind) -> BackendKind {
    other_backend(config, reviewer).unwrap_or(reviewer)
}

/// Asks the verifier about `findings` once. A verifier that fails or answers in the wrong
/// shape leaves every finding in place, marked unverified; only an interrupt ends the review.
async fn verify_findings(
    findings: Vec<Finding>,
    threshold: u8,
    verifier: &mut impl Reviewer,
) -> Result<Applied, CliError> {
    info!(
        step = "verify",
        findings = findings.len(),
        "asking the verifier"
    );
    let verdicts = match verifier.run(None).await {
        Ok(answer) => parse_verdicts(&answer.text, &findings)
            .map_err(|error| info!(step = "verify", %error, "the verifier answer was unusable"))
            .ok(),
        Err(CliError::RunInterrupted) => return Err(CliError::RunInterrupted),
        Err(error) => {
            info!(step = "verify", %error, "the verifier failed");
            None
        }
    };
    Ok(verify::apply(findings, verdicts.as_deref(), threshold))
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
            Err(CliError::RunInterrupted) => return Err(CliError::RunInterrupted),
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

/// Runs an agent in a new session and returns what it said. A direct run is the
/// agent on a session in plan mode, where every permission request is denied; its answer
/// is the text it streamed. Otherwise a session hands the work to the reviewer and the answer
/// is the reviewer's last reply.
async fn run_reviewer(
    ctx: &Context,
    io: &mut Io<'_>,
    on: Backend<'_>,
    cwd: &str,
    request: Request,
    tally: &TokenTally,
) -> Result<Answer, CliError> {
    let Request {
        prompt,
        role,
        direct,
    } = request;
    let agent = role.agent();
    let job = Job {
        prompt,
        cwd: cwd.to_owned(),
        worktree: false,
        backend: Some(on.kind),
        model: on
            .model
            .filter(|_| on.kind.is_model_backend())
            .map(str::to_owned),
        agent: direct.then(|| agent.to_owned()),
        mode: if direct { Mode::Plan } else { Mode::Default },
        max_turns: None,
        token_cap: None,
    };
    let mut reply: Option<String> = None;
    let mut streamed = String::new();
    let mut limit_hit = false;
    let mut watch = |kind: &str, body: &Value| {
        limit_hit |= kind == USAGE_LIMIT_KIND;
        if let Some((input, output)) = usage_tokens(kind, body) {
            tally.add(input, output);
        }
        if direct {
            if let Some(piece) = delta_text(kind, body) {
                streamed.push_str(piece);
            }
        } else if let Some(text) = reply_text(kind, body, agent) {
            reply = Some(text);
        }
    };
    let mut tap = Tap {
        watch: &mut watch,
        print: false,
    };
    let done = drive(ctx, io, false, job, &mut tap).await?;
    if was_cancelled(&done.finish) {
        return Err(CliError::RunInterrupted);
    }
    if let Some(reason) = unavailable_reason(limit_hit, &done.finish) {
        return Err(CliError::BackendUnavailable(reason));
    }
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
        Some(role.answer(&streamed).to_owned()).filter(|text| !text.trim().is_empty())
    } else {
        reply
    };
    let Some(text) = text else {
        return Err(CliError::ReviewFailed(format!(
            "the {agent} agent gave no answer; see the daemon log"
        )));
    };
    Ok(Answer {
        text,
        model: done.model,
    })
}

/// Whether the user, or a daemon that is stopping, cancelled the run. That ends the review:
/// it is not a failure to retry or to keep going after.
fn was_cancelled(finish: &Finish) -> bool {
    finish.condition.as_deref() == Some(CANCELLED)
}

/// The text a streamed piece of a direct run holds, when `kind` and `body` are that event.
fn delta_text<'a>(kind: &str, body: &'a Value) -> Option<&'a str> {
    if kind != STREAM_DELTA_KIND {
        return None;
    }
    body["text"].as_str()
}

/// The text of a reply the agent `agent` wrote, when `kind` and `body` are that event.
fn reply_text(kind: &str, body: &Value, agent: &str) -> Option<String> {
    if kind != CHILD_EVENT_KIND || body["agent"] != agent {
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
    fn write(cwd: &Path, prefix: &str, texts: &[String]) -> Result<Self, CliError> {
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
            let name = input_name(prefix, n);
            let path = dir.join(&name);
            files.relative.push(format!("{INPUT_DIR}/{name}"));
            files.paths.push(path.clone());
            fs::write(&path, text).map_err(|source| CliError::Path { path, source })?;
        }
        Ok(files)
    }
}

/// The file of the part at `index`, from 0. The first keeps the name it always had.
fn input_name(prefix: &str, index: usize) -> String {
    let pid = std::process::id();
    if index == 0 {
        format!("{prefix}-{pid}.txt")
    } else {
        format!("{prefix}-{pid}-{}.txt", index + 1)
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
    fn tokens_come_from_usage_events_that_report_some_and_add_up_over_runs() {
        // Arrange
        let usage = |input: u64, output: u64| serde_json::json!({"usage": {"input_tokens": input, "output_tokens": output}});
        let tally = TokenTally::default();

        // Act
        let none_yet = tally.total();
        for (kind, body) in [
            (USAGE_KIND, usage(100, 10)),
            (USAGE_KIND, usage(0, 0)),
            (STREAM_DELTA_KIND, usage(5, 5)),
            (USAGE_KIND, usage(50, 5)),
        ] {
            if let Some((input, output)) = usage_tokens(kind, &body) {
                tally.add(input, output);
            }
        }

        // Assert
        assert_eq!(none_yet, None);
        assert_eq!(
            tally.total(),
            Some(Tokens {
                input: 150,
                output: 15
            })
        );
        assert_eq!(usage_tokens(USAGE_KIND, &serde_json::json!({})), None);
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
    fn only_a_cancelled_stop_condition_counts_as_an_interrupt() {
        // Arrange
        let finish = |condition: Option<&str>| Finish {
            condition: condition.map(str::to_owned),
            failed: true,
            cut_short: false,
            cause: None,
            hook_reason: None,
        };

        // Act and Assert
        assert!(was_cancelled(&finish(Some(CANCELLED))));
        assert!(!was_cancelled(&finish(Some("max_turns"))));
        assert!(!was_cancelled(&finish(None)));
    }

    #[test]
    fn the_tester_is_known_by_its_agent_name_and_its_reply_is_taken_as_written() {
        // Arrange
        let reply = serde_json::json!({"agent": "tester", "kind": "assistant_message", "body": {"text": "3 passed"}});

        // Act and Assert
        assert_eq!(
            reply_text(CHILD_EVENT_KIND, &reply, Role::Tester.agent()),
            Some("3 passed".to_owned())
        );
        assert_eq!(
            reply_text(CHILD_EVENT_KIND, &reply, Role::Reviewer.agent()),
            None
        );
    }

    #[test]
    fn a_reply_is_the_answer_of_the_agent_it_names_and_of_no_other() {
        // Arrange
        let reply = serde_json::json!({"agent": "verifier", "kind": "assistant_message", "body": {"text": "v"}});

        // Act and Assert
        assert_eq!(
            reply_text(CHILD_EVENT_KIND, &reply, "verifier"),
            Some("v".to_owned())
        );
        assert_eq!(reply_text(CHILD_EVENT_KIND, &reply, "reviewer"), None);
    }

    #[test]
    fn only_the_reviewers_own_reply_is_the_answer() {
        // Arrange
        let reply = |agent: &str, kind: &str| serde_json::json!({"agent": agent, "kind": kind, "body": {"text": "x"}});

        // Act and Assert
        assert_eq!(
            reply_text(
                CHILD_EVENT_KIND,
                &reply("reviewer", "assistant_message"),
                "reviewer"
            ),
            Some("x".to_owned())
        );
        assert_eq!(
            reply_text(
                CHILD_EVENT_KIND,
                &reply("explore", "assistant_message"),
                "reviewer"
            ),
            None
        );
        assert_eq!(
            reply_text(
                CHILD_EVENT_KIND,
                &reply("reviewer", "tool_finished"),
                "reviewer"
            ),
            None
        );
        assert_eq!(
            reply_text(
                "assistant_message",
                &serde_json::json!({"text": "x"}),
                "reviewer"
            ),
            None
        );
    }

    #[test]
    fn the_input_files_are_removed_with_the_directories_they_made() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let texts = ["one".to_owned(), "two".to_owned()];

        // Act
        let files = InputFiles::write(dir.path(), INPUT_FILE_PREFIX, &texts).unwrap();
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
        assert_eq!(input_name("input", 0), format!("input-{pid}.txt"));
        assert_eq!(input_name("input", 1), format!("input-{pid}-2.txt"));
        assert_eq!(input_name("verify", 0), format!("verify-{pid}.txt"));
    }

    #[test]
    fn an_existing_coda_directory_stays() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(INPUT_PARENT_DIR)).unwrap();

        // Act
        drop(InputFiles::write(dir.path(), INPUT_FILE_PREFIX, &["text".to_owned()]).unwrap());

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

    /// A backend that fails with `error`, or answers when it has none, counting its asks.
    struct Flaky {
        error: Option<Unavailable>,
        asked: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Attempt for Flaky {
        fn attempt(
            &mut self,
            _: BackendKind,
            _: Option<&str>,
            _: Option<String>,
        ) -> impl Future<Output = Result<Answer, CliError>> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(match self.error {
                Some(why) => Err(CliError::BackendUnavailable(why)),
                None => Ok(Answer {
                    text: GOOD.to_owned(),
                    model: None,
                }),
            })
        }
    }

    fn only_claude(
        error: Option<Unavailable>,
    ) -> (
        Failover<Flaky>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backend = Flaky {
            error,
            asked: std::sync::Arc::clone(&asked),
        };
        let part = Failover::new(backend, vec![BackendKind::ClaudeCode], None, String::new());
        (part, asked)
    }

    #[tokio::test]
    async fn a_part_with_no_backend_left_is_reported_failed_and_its_files_are_left_out() {
        // Arrange
        let parts = two_parts();
        let (limited, _) = only_claude(Some(Unavailable::UsageLimit));
        let (fine, _) = only_claude(None);
        let mut reviewers = vec![limited, fine];
        let results = review_parts(&parts, &mut reviewers, 2).await;
        let given = vec!["a.txt".to_owned(), "b.txt".to_owned()];

        // Act
        let (merged, _) = merged_review(&parts, results, &given, 5).unwrap();

        // Assert
        assert!(merged.partial);
        assert_eq!(merged.coverage.left_out, ["a.txt"]);
    }

    #[tokio::test]
    async fn a_usage_limit_is_not_retried_on_the_same_backend_but_a_lost_backend_is() {
        // Arrange
        let (mut limited, limited_asked) = only_claude(Some(Unavailable::UsageLimit));
        let (mut lost, lost_asked) = only_claude(Some(Unavailable::Lost));

        // Act
        let limit_result = review_part(&packed().scope, &mut limited).await;
        let lost_result = review_part(&packed().scope, &mut lost).await;

        // Assert
        assert!(matches!(
            limit_result,
            Err(CliError::BackendUnavailable(Unavailable::UsageLimit))
        ));
        assert!(matches!(
            lost_result,
            Err(CliError::BackendUnavailable(Unavailable::Lost))
        ));
        assert_eq!(limited_asked.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(lost_asked.load(std::sync::atomic::Ordering::SeqCst), 2);
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
    async fn an_interrupted_part_is_not_tried_again_and_ends_the_review() {
        // Arrange
        struct Interrupted(usize);
        impl Reviewer for Interrupted {
            fn run(
                &mut self,
                _hint: Option<String>,
            ) -> impl Future<Output = Result<Answer, CliError>> {
                self.0 += 1;
                std::future::ready(Err(CliError::RunInterrupted))
            }
        }
        let mut reviewer = Interrupted(0);
        let results = vec![Ok((parsed_of(GOOD), None)), Err(CliError::RunInterrupted)];

        // Act
        let part = review_part(&packed().scope, &mut reviewer).await;
        let parts = two_parts();
        let collected = collect(&parts, results);

        // Assert
        assert!(matches!(part, Err(CliError::RunInterrupted)));
        assert_eq!(reviewer.0, 1);
        assert!(matches!(collected, Err(CliError::RunInterrupted)));
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
            let mut reviewers: Vec<Counting> = (0..5)
                .map(|_| Counting {
                    running: Arc::clone(&running),
                    most: Arc::clone(&most),
                })
                .collect();

            // Act
            let results = review_parts(&parts, &mut reviewers, limit).await;

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
        let mut reviewers = vec![
            scripted(&[Err("first"), Err("first")]),
            scripted(&[Ok(GOOD)]),
        ];

        // Act
        let results = review_parts(&parts, &mut reviewers, 2).await;

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
        let (merged, model) = merged_review(&parts, results, &given, 5).unwrap();
        let review = assemble(merged, "claude_code", 10, None);
        let text = to_markdown(&review);

        // Assert
        assert_eq!(model, None);
        assert!(review.run.partial);
        assert_eq!(review.run.chunks, 2);
        assert_eq!(review.findings.len(), 1);
        assert_eq!(review.coverage.reviewed, ["a.txt"]);
        assert_eq!(review.coverage.left_out, ["b.txt"]);
        assert!(
            text.starts_with("# Review\n\n**Partial review:**"),
            "{text}"
        );
    }

    fn enabled(keys: &[&str]) -> Config {
        let toml = keys.iter().fold(String::new(), |mut text, key| {
            let _ = std::fmt::Write::write_fmt(
                &mut text,
                format_args!("[providers.{key}]\nenabled = true\n"),
            );
            text
        });
        Config::from_toml_str(&toml).unwrap().config
    }

    #[test]
    fn the_verifier_runs_on_the_other_backend_when_both_are_enabled_else_on_the_same() {
        // Arrange
        let both = enabled(&["claude_code", "codex"]);
        let only_claude = enabled(&["claude_code"]);
        let only_codex = enabled(&["codex"]);

        // Act and Assert
        assert_eq!(
            verifier_backend(&both, BackendKind::ClaudeCode),
            BackendKind::CodexAppServer
        );
        assert_eq!(
            verifier_backend(&both, BackendKind::CodexAppServer),
            BackendKind::ClaudeCode
        );
        assert_eq!(
            verifier_backend(&both, BackendKind::CodexExec),
            BackendKind::ClaudeCode
        );
        assert_eq!(
            verifier_backend(&only_claude, BackendKind::ClaudeCode),
            BackendKind::ClaudeCode
        );
        assert_eq!(
            verifier_backend(&only_codex, BackendKind::CodexAppServer),
            BackendKind::CodexAppServer
        );
        assert_eq!(
            verifier_backend(&both, BackendKind::Anthropic),
            BackendKind::Anthropic
        );
        assert_eq!(
            verifier_backend(&both, BackendKind::Faux),
            BackendKind::Faux
        );
    }

    fn two_findings() -> Vec<Finding> {
        let one = |id: &str, line: u32| {
            let text = format!(
                r#"{{"summary": "s", "effort": 1, "risk": "low", "findings": [{{"file": "a.txt", "start_line": {line}, "end_line": {line}, "severity": "high", "category": "bug", "title": "T{id}", "explanation": "E", "confidence": "high"}}]}}"#
            );
            let mut finding = parsed_of(&text).findings.remove(0);
            finding.id = id.to_owned();
            finding
        };
        vec![one("F1", 1), one("F2", 2)]
    }

    const VERDICTS: &str = r#"{"verdicts": [{"id": "F1", "score": 0, "reason": "false"}, {"id": "F2", "score": 8, "reason": "real"}]}"#;

    #[tokio::test]
    async fn the_verifier_drops_a_false_finding_scores_the_rest_and_is_asked_once() {
        // Arrange
        let mut verifier = scripted(&[Ok(VERDICTS), Ok(VERDICTS)]);

        // Act
        let applied = verify_findings(two_findings(), 4, &mut verifier)
            .await
            .unwrap();

        // Assert
        assert_eq!(applied.dropped, 1);
        assert_eq!(applied.findings.len(), 1);
        assert_eq!(applied.findings[0].title, "TF2");
        assert_eq!(applied.findings[0].score, Some(8));
        assert_eq!(verifier.hints.len(), 1);
    }

    #[tokio::test]
    async fn a_verifier_that_fails_or_answers_junk_leaves_the_findings_unverified() {
        for reply in [Err("down"), Ok("prose"), Ok(r#"{"verdicts": 3}"#)] {
            // Arrange
            let mut verifier = scripted(&[reply]);

            // Act
            let applied = verify_findings(two_findings(), 4, &mut verifier)
                .await
                .unwrap();

            // Assert
            assert_eq!(applied.dropped, 0, "{reply:?}");
            assert_eq!(applied.findings.len(), 2, "{reply:?}");
            assert!(applied.findings.iter().all(|f| f.unverified), "{reply:?}");
            assert!(
                applied.findings.iter().all(|f| f.score.is_none()),
                "{reply:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_interrupt_during_the_verifier_ends_the_review() {
        // Arrange
        struct Interrupted;
        impl Reviewer for Interrupted {
            fn run(
                &mut self,
                _hint: Option<String>,
            ) -> impl Future<Output = Result<Answer, CliError>> {
                std::future::ready(Err(CliError::RunInterrupted))
            }
        }

        // Act
        let result = verify_findings(two_findings(), 4, &mut Interrupted).await;

        // Assert
        assert!(matches!(result, Err(CliError::RunInterrupted)));
    }
}
