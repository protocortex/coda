// SPDX-License-Identifier: Apache-2.0

//! `coda describe`: a draft title, summary and file notes for a change, printed for the user.
//! With `--apply` and the user's confirmation, the draft goes in the pull request's body, and
//! the suggested labels are added. It only runs `gh`, as the user's own login.

use std::path::Path;
use std::process::ExitCode;

use coda_config::describe::LabelRule;
use coda_protocol::types::BackendKind;
use coda_review::describe::{BadDraft, Choice, Draft, block, parse_draft, prompt, replace_block};
use coda_review::diff::parse_diff;
use coda_review::request::{Role, Target};
use serde_json::Value;

use crate::autostart::ensure_daemon;
use crate::cli::DescribeArgs;
use crate::context::{Context, Io};
use crate::daemon_cmd::load_config;
use crate::error::CliError;
use crate::review::{Ask, change_text, load_rules, review_backend, split_files};
use crate::review_post::{Confirm, Gh, RealGh, asked_yes};
use crate::review_route::Attempt;
use crate::run::session_cwd;

/// Starts the name of the file a model-API run reads.
const FILE_PREFIX: &str = "describe";
/// A draft is asked for again once when its answer cannot be used.
const MAX_ATTEMPTS: usize = 2;
const NOTHING_TO_DESCRIBE: &str = "nothing to describe: the change has no lines to look at";
const NOTHING_LEFT: &str =
    "nothing to describe: every changed file was left out by the review rules";
const PR_FIELDS: &str = "body,labels";
const LABEL_LIST_LIMIT: &str = "200";
const LABEL_LIST_LIMIT_COUNT: usize = 200;
/// `gh` splits the value of `--add-label` here, and reads quotes in it as CSV.
const LIST_SEPARATOR: char = ',';
const QUOTE: char = '"';
const URL_PULL_SEGMENT: &str = "pull";
const APPLY_QUESTION: &str = "Update the pull request with this? [y/N] ";

/// Runs the command.
///
/// # Errors
/// Returns a [`CliError`] when the change cannot be read, the describer fails or does not
/// answer in the format twice, or `--apply` cannot reach the pull request.
pub async fn run(ctx: &Context, io: &mut Io<'_>, args: DescribeArgs) -> Result<ExitCode, CliError> {
    let config = load_config(&ctx.dirs)?;
    let backend = review_backend(&config, args.backend.map(Into::into));
    let cwd = session_cwd(&ctx.cwd, args.cwd.clone())?;
    let repo = Path::new(&cwd);
    let target = (!args.staged).then(|| Target::parse(args.target.as_deref()));
    let apply = apply_request(ctx, &args, target.as_ref())?;
    let rules = load_rules(&config.review, repo)?;
    let files = parse_diff(&change_text(repo, target.as_ref())?);
    if files.is_empty() {
        writeln!(io.out, "{NOTHING_TO_DESCRIBE}")?;
        return Ok(ExitCode::SUCCESS);
    }
    let parts = split_files(repo, &files, &config.review, &rules)?;
    let part = &parts[0];
    if part.coverage.reviewed.is_empty() && part.coverage.clipped.is_empty() {
        writeln!(io.out, "{NOTHING_LEFT}")?;
        return Ok(ExitCode::SUCCESS);
    }
    let left_out = part.coverage.left_out.len() + parts.len().saturating_sub(1);
    if left_out > 0 {
        writeln!(
            io.err,
            "coda: the draft is of the first part of a large change; files left out or in later parts: {left_out}"
        )?;
    }
    let allowed = if args.labels {
        let from = target.as_ref().and_then(|t| match t {
            Target::PullRequest(pr) => repo_of_url(pr),
            _ => None,
        });
        let (allowed, note) = allowed_labels(
            &mut RealGh { repo },
            &config.describe.labels,
            from.as_deref(),
        );
        if let Some(text) = note {
            writeln!(io.err, "coda: {text}")?;
        }
        allowed
    } else {
        Vec::new()
    };
    let input = prompt(&part.text, &allowed, args.instructions.as_deref());
    drop(ensure_daemon(ctx, io, false).await?);
    let mut asker = Ask {
        ctx,
        cwd: &cwd,
        role: Role::Describer,
        file_prefix: FILE_PREFIX,
        input: &input,
    };
    let names: Vec<String> = files.iter().map(|f| f.path.clone()).collect();
    let draft = ask_draft(&mut asker, backend, &names, &allowed).await?;
    print_draft(io, &draft, args.json, args.labels)?;
    if let Some((pr, confirm)) = apply {
        apply_draft(&mut RealGh { repo }, io, confirm, &pr, &draft, args.labels)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// What `--apply` writes to and how it is confirmed, checked before any model run is paid
/// for. `None` without `--apply`.
fn apply_request(
    ctx: &Context,
    args: &DescribeArgs,
    target: Option<&Target>,
) -> Result<Option<(String, Confirm)>, CliError> {
    if !args.apply {
        return Ok(None);
    }
    let confirm = Confirm {
        assume_yes: args.yes,
        interactive: ctx.stdin_is_tty,
    };
    confirm.check().map_err(local)?;
    match target {
        Some(Target::PullRequest(pr)) => Ok(Some((pr.clone(), confirm))),
        _ => Err(CliError::Describe(
            "--apply needs a pull request to write to; name one by number or URL".to_owned(),
        )),
    }
}

/// The labels that may be suggested: those of `rules` that the repository has, spelled as the
/// repository spells them. The note says why there are none, when that is so.
fn allowed_labels(
    gh: &mut impl Gh,
    rules: &[LabelRule],
    repo: Option<&str>,
) -> (Vec<Choice>, Option<String>) {
    if rules.is_empty() {
        return (
            Vec::new(),
            Some("no labels are suggested: the config has no [[describe.labels]]".to_owned()),
        );
    }
    let mut args = vec![
        "label",
        "list",
        "--json",
        "name",
        "--limit",
        LABEL_LIST_LIMIT,
    ];
    if let Some(repo) = repo {
        args.extend(["--repo", repo]);
    }
    let listed = gh
        .run(&args, None)
        .ok()
        .and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok());
    let Some(listed) = listed else {
        return (
            Vec::new(),
            Some("no labels are suggested: the repository's labels could not be read".to_owned()),
        );
    };
    let repo_names: Vec<&str> = listed.iter().filter_map(|l| l["name"].as_str()).collect();
    let (usable, unusable): (Vec<&str>, Vec<&str>) = repo_names
        .iter()
        .partition(|name| !name.contains(LIST_SEPARATOR) && !name.contains(QUOTE));
    let allowed = rules
        .iter()
        .filter_map(|rule| {
            let wanted = rule.name.trim().to_lowercase();
            usable
                .iter()
                .find(|name| name.to_lowercase() == wanted)
                .map(|name| Choice {
                    name: (*name).to_owned(),
                    use_when: rule.use_when.trim().to_owned(),
                })
        })
        .collect();
    let mut notes: Vec<String> = Vec::new();
    let skipped = rules.iter().any(|rule| {
        let wanted = rule.name.trim().to_lowercase();
        unusable.iter().any(|name| name.to_lowercase() == wanted)
    });
    if skipped {
        notes.push(
            "a label with a comma or a quote in its name is not suggested, since `gh` cannot add it"
                .to_owned(),
        );
    }
    if listed.len() >= LABEL_LIST_LIMIT_COUNT {
        notes.push(format!(
            "the repository has at least {LABEL_LIST_LIMIT_COUNT} labels and only that many were read"
        ));
    }
    (allowed, (!notes.is_empty()).then(|| notes.join("; ")))
}

/// `owner/repo` of a pull request URL, so `gh` reads the labels of that repository and not
/// of the folder it runs in.
fn repo_of_url(pr: &str) -> Option<String> {
    let rest = pr.split_once("://")?.1;
    let mut parts = rest.split('/');
    let (_host, owner, repo, marker) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    (marker == URL_PULL_SEGMENT && !owner.is_empty() && !repo.is_empty())
        .then(|| format!("{owner}/{repo}"))
}

/// A pull request or `gh` error, said as this command's.
fn local(error: CliError) -> CliError {
    match error {
        CliError::ReviewPost(why) => CliError::Describe(why),
        other => other,
    }
}

/// Asks the describer until its answer parses, at most [`MAX_ATTEMPTS`] times. The second
/// ask carries the hint that says what was wrong.
async fn ask_draft(
    asker: &mut impl Attempt,
    backend: BackendKind,
    files: &[String],
    allowed: &[Choice],
) -> Result<Draft, CliError> {
    let mut hint: Option<String> = None;
    let mut last = BadDraft(String::new());
    for _ in 0..MAX_ATTEMPTS {
        let answer = asker.attempt(backend, None, hint.take()).await?;
        match parse_draft(&answer.text, files, allowed) {
            Ok(draft) => return Ok(draft),
            Err(bad) => {
                hint = Some(bad.retry_hint());
                last = bad;
            }
        }
    }
    Err(CliError::Describe(format!(
        "the describer did not give a usable answer: {last}"
    )))
}

fn print_draft(
    io: &mut Io<'_>,
    draft: &Draft,
    json: bool,
    labels_asked: bool,
) -> Result<(), CliError> {
    if json {
        let text =
            serde_json::to_string_pretty(draft).map_err(|e| CliError::Describe(e.to_string()))?;
        writeln!(io.out, "{text}")?;
        return Ok(());
    }
    writeln!(
        io.out,
        "Title: {}\n\nSummary\n\n{}",
        draft.title, draft.summary
    )?;
    if !draft.files.is_empty() {
        writeln!(io.out, "\nFiles\n")?;
        for note in &draft.files {
            writeln!(io.out, "- {}: {}", note.file, note.note)?;
        }
    }
    if !draft.labels.is_empty() {
        writeln!(io.out, "\nSuggested labels: {}", draft.labels.join(", "))?;
    }
    if labels_asked && draft.dropped_labels > 0 {
        writeln!(
            io.err,
            "coda: {} suggested label(s) were not on the allowed list and are left out",
            draft.dropped_labels
        )?;
    }
    Ok(())
}

/// Puts the draft's block in the body of pull request `pr`, and adds the labels the pull
/// request lacks when `with_labels` is set, after a preview and a yes. It never removes a
/// label and never changes the title or the text outside the block.
fn apply_draft(
    gh: &mut impl Gh,
    io: &mut Io<'_>,
    confirm: Confirm,
    pr: &str,
    draft: &Draft,
    with_labels: bool,
) -> Result<(), CliError> {
    let now = gh
        .run(&["pr", "view", pr, "--json", PR_FIELDS], None)
        .map_err(local)?;
    let now: Value = serde_json::from_str(&now)
        .map_err(|e| CliError::Describe(format!("cannot read the pull request: {e}")))?;
    let body = now["body"].as_str().unwrap_or_default();
    let new_body =
        replace_block(body, &block(draft)).map_err(|e| CliError::Describe(e.to_string()))?;
    let have: Vec<String> = now["labels"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(|l| l["name"].as_str())
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default();
    let add: Vec<&String> = if with_labels {
        draft
            .labels
            .iter()
            .filter(|label| !have.contains(&label.to_lowercase()))
            .collect()
    } else {
        Vec::new()
    };
    if new_body == body && add.is_empty() {
        writeln!(
            io.err,
            "coda: nothing to change: the pull request already has this"
        )?;
        return Ok(());
    }
    writeln!(
        io.err,
        "The pull request body will hold this block, and the text outside it stays:\n"
    )?;
    writeln!(io.err, "{}\n", block(draft))?;
    if !add.is_empty() {
        let names: Vec<&str> = add.iter().map(|l| l.as_str()).collect();
        writeln!(
            io.err,
            "Labels to add (none is removed): {}\n",
            names.join(", ")
        )?;
    }
    if !confirm.assume_yes && !asked_yes(io, APPLY_QUESTION)? {
        writeln!(io.err, "coda: nothing was written")?;
        return Ok(());
    }
    let mut args = vec!["pr", "edit", pr];
    if new_body != body {
        args.extend(["--body-file", "-"]);
    }
    for label in &add {
        args.extend(["--add-label", label.as_str()]);
    }
    let stdin = (new_body != body).then_some(new_body.as_str());
    gh.run(&args, stdin).map_err(local)?;
    writeln!(io.err, "coda: pull request updated")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io::Cursor;

    use coda_review::describe::{BLOCK_END, BLOCK_START, FileNote};

    use super::*;
    use crate::review::Answer;

    fn draft() -> Draft {
        Draft {
            title: "Fix the parser".to_owned(),
            summary: "It read one item too far.".to_owned(),
            files: vec![FileNote {
                file: "src/a.rs".to_owned(),
                note: "stops at the end".to_owned(),
            }],
            labels: vec!["bug".to_owned(), "docs".to_owned()],
            dropped_labels: 0,
        }
    }

    fn rule(name: &str) -> LabelRule {
        LabelRule {
            name: name.to_owned(),
            use_when: "it fits".to_owned(),
        }
    }

    /// A `gh` that answers `view` with `pr_json` and records every other call.
    struct Fake {
        view: String,
        lists: Vec<Vec<String>>,
        calls: Vec<(Vec<String>, Option<String>)>,
    }

    impl Gh for Fake {
        fn run(&mut self, args: &[&str], stdin: Option<&str>) -> Result<String, CliError> {
            if args.starts_with(&["label", "list"]) {
                self.lists
                    .push(args.iter().map(|a| (*a).to_owned()).collect());
                return Ok(self.view.clone());
            }
            if args.starts_with(&["pr", "view"]) {
                return Ok(self.view.clone());
            }
            self.calls.push((
                args.iter().map(|a| (*a).to_owned()).collect(),
                stdin.map(str::to_owned),
            ));
            Ok(String::new())
        }
    }

    fn fake(body: &str, labels: &[&str]) -> Fake {
        let labels: Vec<Value> = labels
            .iter()
            .map(|l| serde_json::json!({"name": l}))
            .collect();
        Fake {
            view: serde_json::json!({"body": body, "labels": labels}).to_string(),
            lists: Vec::new(),
            calls: Vec::new(),
        }
    }

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

    const ASK: Confirm = Confirm {
        assume_yes: false,
        interactive: true,
    };
    const YES: Confirm = Confirm {
        assume_yes: true,
        interactive: false,
    };

    #[test]
    fn a_confirmed_apply_replaces_only_the_block_and_adds_only_the_missing_labels() {
        // Arrange
        let old = format!("{BLOCK_START}\nold\n{BLOCK_END}");
        let mut gh = fake(
            &format!("My notes.\n\n{old}\n\nMy footer.\n"),
            &["Bug", "other"],
        );
        let mut streams = Streams::new("y\n");

        // Act
        apply_draft(&mut gh, &mut streams.io(), ASK, "7", &draft(), true).unwrap();

        // Assert
        assert_eq!(gh.calls.len(), 1, "one edit, all or nothing");
        let (edit_args, body) = &gh.calls[0];
        assert_eq!(
            edit_args,
            &["pr", "edit", "7", "--body-file", "-", "--add-label", "docs"]
        );
        let body = body.as_deref().unwrap();
        assert!(body.starts_with("My notes.\n\n"), "{body}");
        assert!(body.ends_with("\n\nMy footer.\n"), "{body}");
        assert!(body.contains("It read one item too far."));
        assert!(!body.contains("old"));
    }

    #[test]
    fn declining_writes_nothing_and_yes_skips_the_question() {
        // Arrange
        let mut declined = fake("Mine.", &[]);
        let mut said_no = Streams::new("n\n");
        let mut assumed = fake("Mine.", &[]);
        let mut quiet = Streams::new("");

        // Act
        apply_draft(&mut declined, &mut said_no.io(), ASK, "7", &draft(), true).unwrap();
        apply_draft(&mut assumed, &mut quiet.io(), YES, "7", &draft(), false).unwrap();

        // Assert
        assert_eq!(declined.calls.len(), 0);
        assert!(String::from_utf8_lossy(&said_no.err).contains("nothing was written"));
        assert_eq!(
            assumed.calls.len(),
            1,
            "labels are left alone without --labels"
        );
        assert!(!String::from_utf8_lossy(&quiet.err).contains("Update the pull request"));
    }

    #[test]
    fn a_lone_marker_stops_the_apply_before_anything_is_written() {
        // Arrange
        let mut gh = fake(&format!("text {BLOCK_START} more"), &[]);
        let mut streams = Streams::new("y\n");

        // Act
        let error = apply_draft(&mut gh, &mut streams.io(), YES, "7", &draft(), true).unwrap_err();

        // Assert
        assert!(matches!(error, CliError::Describe(_)), "{error}");
        assert_eq!(gh.calls.len(), 0);
    }

    #[test]
    fn a_pull_request_that_already_has_the_draft_and_the_labels_is_left_alone() {
        // Arrange
        let body = replace_block("Mine.", &block(&draft())).unwrap();
        let mut gh = fake(&body, &["bug", "docs"]);
        let mut streams = Streams::new("y\n");

        // Act
        apply_draft(&mut gh, &mut streams.io(), ASK, "7", &draft(), true).unwrap();

        // Assert
        assert_eq!(gh.calls.len(), 0);
        assert!(String::from_utf8_lossy(&streams.err).contains("nothing to change"));
    }

    #[test]
    fn the_allowed_labels_are_the_configured_ones_the_repository_has_in_its_spelling() {
        // Arrange
        let mut gh = fake("", &[]);
        gh.view =
            serde_json::json!([{"name": "Bug"}, {"name": "docs"}, {"name": "wip"}]).to_string();
        let rules = [rule("bug"), rule("security"), rule("DOCS")];

        // Act
        let (allowed, note) = allowed_labels(&mut gh, &rules, None);

        // Assert
        let names: Vec<&str> = allowed.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Bug", "docs"]);
        assert_eq!(note, None);
    }

    #[test]
    fn no_configured_label_or_an_unreadable_label_list_means_no_label_and_says_why() {
        // Arrange
        let mut broken = fake("", &[]);
        broken.view = "not json".to_owned();

        // Act
        let (none, none_note) = allowed_labels(&mut fake("", &[]), &[], None);
        let (unread, unread_note) = allowed_labels(&mut broken, &[rule("bug")], None);

        // Assert
        assert_eq!(none.len(), 0);
        assert!(none_note.unwrap().contains("[[describe.labels]]"));
        assert_eq!(unread.len(), 0);
        assert!(unread_note.unwrap().contains("could not be read"));
    }

    /// Answers in turn and remembers the hints it was given.
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

    const GOOD: &str = r#"{"title": "t", "summary": "s"}"#;

    #[tokio::test]
    async fn a_bad_answer_is_asked_for_again_once_with_a_hint_and_a_second_one_is_an_error() {
        // Arrange
        let mut recovers = Scripted {
            replies: vec!["prose", GOOD],
            hints: Vec::new(),
        };
        let mut fails = Scripted {
            replies: vec!["prose", "more prose", GOOD],
            hints: Vec::new(),
        };

        // Act
        let draft = ask_draft(&mut recovers, BackendKind::Faux, &[], &[])
            .await
            .unwrap();
        let error = ask_draft(&mut fails, BackendKind::Faux, &[], &[])
            .await
            .unwrap_err();

        // Assert
        assert_eq!(draft.title, "t");
        assert!(recovers.hints[1].as_ref().unwrap().contains("rejected"));
        assert!(matches!(error, CliError::Describe(_)), "{error}");
        assert_eq!(fails.hints.len(), 2);
    }

    #[test]
    fn the_printed_draft_names_the_title_summary_files_and_labels() {
        // Arrange
        let mut streams = Streams::new("");
        let mut with_dropped = draft();
        with_dropped.dropped_labels = 2;

        // Act
        print_draft(&mut streams.io(), &with_dropped, false, true).unwrap();
        let mut quiet = Streams::new("");
        print_draft(&mut quiet.io(), &with_dropped, false, false).unwrap();

        // Assert
        let out = String::from_utf8_lossy(&streams.out).into_owned();
        assert!(out.starts_with("Title: Fix the parser\n\nSummary\n\nIt read one item too far."));
        assert!(out.contains("- src/a.rs: stops at the end"));
        assert!(out.contains("Suggested labels: bug, docs"));
        assert!(String::from_utf8_lossy(&streams.err).contains("2 suggested label(s)"));
        assert_eq!(
            String::from_utf8_lossy(&quiet.err),
            "",
            "no warning without --labels"
        );
    }

    #[test]
    fn a_label_gh_cannot_add_is_not_suggested_and_the_note_says_so() {
        // Arrange
        let mut gh = fake("", &[]);
        gh.view =
            serde_json::json!([{"name": "needs, review"}, {"name": "say \"hi\""}, {"name": "bug"}])
                .to_string();
        let rules = [rule("needs, review"), rule("say \"hi\""), rule("bug")];

        // Act
        let (allowed, note) = allowed_labels(&mut gh, &rules, None);

        // Assert
        let names: Vec<&str> = allowed.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["bug"]);
        assert!(note.unwrap().contains("comma or a quote"));
    }

    #[test]
    fn labels_are_read_from_the_repository_of_a_pull_request_url() {
        // Arrange
        let mut gh = fake("", &[]);
        gh.view = serde_json::json!([{"name": "bug"}]).to_string();

        // Act
        let from = repo_of_url("https://github.com/acme/lib/pull/5");
        allowed_labels(&mut gh, &[rule("bug")], from.as_deref());
        allowed_labels(&mut gh, &[rule("bug")], repo_of_url("7").as_deref());

        // Assert
        assert_eq!(from.as_deref(), Some("acme/lib"));
        assert_eq!(
            &gh.lists[0][gh.lists[0].len() - 2..],
            ["--repo", "acme/lib"]
        );
        assert!(!gh.lists[1].contains(&"--repo".to_owned()));
        assert_eq!(repo_of_url("https://github.com/acme/lib/issues/5"), None);
    }

    #[test]
    fn a_full_label_list_says_it_may_be_cut() {
        // Arrange
        let mut gh = fake("", &[]);
        let many: Vec<Value> = (0..LABEL_LIST_LIMIT_COUNT)
            .map(|n| serde_json::json!({"name": format!("l{n}")}))
            .collect();
        gh.view = Value::Array(many).to_string();

        // Act
        let (_, note) = allowed_labels(&mut gh, &[rule("l1")], None);

        // Assert
        assert!(note.unwrap().contains("only that many were read"));
    }

    #[test]
    fn the_only_calls_apply_makes_are_one_view_and_at_most_one_add_only_edit() {
        // Arrange
        let mut gh = fake("Mine.", &["keep"]);
        let mut streams = Streams::new("");

        // Act
        apply_draft(&mut gh, &mut streams.io(), YES, "7", &draft(), true).unwrap();

        // Assert
        assert_eq!(gh.calls.len(), 1);
        let args = &gh.calls[0].0;
        assert_eq!(
            args,
            &[
                "pr",
                "edit",
                "7",
                "--body-file",
                "-",
                "--add-label",
                "bug",
                "--add-label",
                "docs"
            ]
        );
    }

    #[test]
    fn a_pull_request_error_is_said_as_this_commands() {
        // Act
        let local = local(CliError::ReviewPost("`gh` failed: no access".to_owned()));

        // Assert
        assert_eq!(
            local.to_string(),
            "cannot describe the change: `gh` failed: no access"
        );
    }
}
