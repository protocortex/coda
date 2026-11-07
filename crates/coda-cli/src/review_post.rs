// SPDX-License-Identifier: Apache-2.0

//! `coda review --post`: turns a finished review into a pending GitHub review with inline
//! comments and one editable summary comment. It only runs `gh`, so it acts as the user's own
//! `gh` login and reads no token. It never approves or requests changes: the pending review
//! stays pending until the user submits it on GitHub.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use coda_review::post::{InlineComment, PostPlan, SUMMARY_MARKER, fingerprint_in, plan, preview};
use coda_review::schema::{DiffScope, Review};
use serde_json::{Value, json};

use crate::context::Io;
use crate::error::CliError;

const GH: &str = "gh";
const PR_FIELDS: &str = "number,url,headRefOid";
const PENDING: &str = "PENDING";
const RIGHT_SIDE: &str = "RIGHT";
const URL_PULL_MARKER: &str = "/pull/";
const URL_PATH_SEGMENTS: usize = 2;
const YES_ANSWERS: [&str; 2] = ["y", "yes"];

/// Runs `gh`. A test replaces it with a closure.
pub trait Gh {
    /// Runs `gh` with `args` and `stdin`, and returns what it printed.
    ///
    /// # Errors
    /// Returns [`CliError::ReviewPost`] when `gh` cannot run or exits with a failure.
    fn run(&mut self, args: &[&str], stdin: Option<&str>) -> Result<String, CliError>;
}

impl<F> Gh for F
where
    F: FnMut(&[&str], Option<&str>) -> Result<String, CliError>,
{
    fn run(&mut self, args: &[&str], stdin: Option<&str>) -> Result<String, CliError> {
        self(args, stdin)
    }
}

/// The real `gh`, run in `repo`.
pub struct RealGh<'a> {
    /// The directory `gh` runs in.
    pub repo: &'a Path,
}

impl Gh for RealGh<'_> {
    fn run(&mut self, args: &[&str], stdin: Option<&str>) -> Result<String, CliError> {
        let fail = |why: String| CliError::ReviewPost(why);
        let mut child = Command::new(GH)
            .args(args)
            .current_dir(self.repo)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| fail(format!("could not run `{GH}`: {e}")))?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            pipe.write_all(text.as_bytes())
                .map_err(|e| fail(format!("could not write to `{GH}`: {e}")))?;
        }
        let out = child
            .wait_with_output()
            .map_err(|e| fail(format!("could not run `{GH}`: {e}")))?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr).trim().to_owned();
            return Err(fail(format!("`{GH}` failed: {why}")));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// How the user's confirmation is got.
#[derive(Debug, Clone, Copy)]
pub struct Confirm {
    /// The user passed `--yes`.
    pub assume_yes: bool,
    /// Standard input is a terminal, so the user can be asked.
    pub interactive: bool,
}

impl Confirm {
    /// Refuses a run that could never be confirmed.
    ///
    /// # Errors
    /// Returns [`CliError::ReviewPost`] when there is no `--yes` and nobody to ask.
    pub fn check(self) -> Result<(), CliError> {
        if self.assume_yes || self.interactive {
            return Ok(());
        }
        Err(CliError::ReviewPost(
            "standard input is not a terminal, so pass --yes to post without being asked"
                .to_owned(),
        ))
    }
}

/// Posts `review` to the pull request `pr`. Findings on a line of the diff of one of
/// `scopes` become comments of one pending review; the others go in a summary comment that
/// a later run edits.
///
/// # Errors
/// Returns [`CliError::ReviewPost`] when `gh` fails, the pull request cannot be read, or the
/// user already has a pending review there.
pub fn post(
    gh: &mut impl Gh,
    io: &mut Io<'_>,
    confirm: Confirm,
    pr: &str,
    reviewed_head: &str,
    review: &Review,
    scopes: &[&DiffScope],
) -> Result<(), CliError> {
    let target = read_target(gh, pr)?;
    if target.head_sha != reviewed_head {
        return Err(CliError::ReviewPost(
            "the pull request changed since the review began, so its lines may have moved; \
             run the review again"
                .to_owned(),
        ));
    }
    let me = login(gh)?;
    let theirs = |item: &Value| item["user"]["login"].as_str() == Some(me.as_str());
    let base = &target.repo_path;
    let number = target.number;
    let mut posted: BTreeSet<String> = BTreeSet::new();
    let mut note_posted = |items: &[Value]| {
        for item in items.iter().filter(|i| theirs(i)) {
            let found = item["body"].as_str().and_then(fingerprint_in);
            posted.extend(found.map(str::to_owned));
        }
    };
    note_posted(&list(gh, &format!("{base}/pulls/{number}/comments"))?);
    let reviews = list(gh, &format!("{base}/pulls/{number}/reviews"))?;
    let pending = reviews
        .iter()
        .find(|r| r["state"] == PENDING && theirs(r))
        .and_then(|r| r["id"].as_u64());
    if let Some(id) = pending {
        note_posted(&list(
            gh,
            &format!("{base}/pulls/{number}/reviews/{id}/comments"),
        )?);
    }
    let issue_comments = list(gh, &format!("{base}/issues/{number}/comments"))?;
    let summary = issue_comments.iter().filter(|c| theirs(c)).find(|c| {
        c["body"]
            .as_str()
            .is_some_and(|b| b.contains(SUMMARY_MARKER))
    });
    let summary_id = summary.and_then(|c| c["id"].as_u64());
    let summary_body = summary.and_then(|c| c["body"].as_str());

    let plan = plan(review, scopes, &posted);
    if pending.is_some() && !plan.inline.is_empty() {
        return Err(CliError::ReviewPost(
            "you already have a pending review on this pull request; submit or discard it, \
             then run this again"
                .to_owned(),
        ));
    }
    let summary_changed = summary_body != Some(plan.summary.as_str());
    if plan.inline.is_empty() && !summary_changed {
        writeln!(
            io.err,
            "coda: nothing to post: every comment is already there"
        )?;
        return Ok(());
    }
    write!(io.err, "{}", preview(&plan, summary_id.is_some()))?;
    if !confirm.assume_yes && !asked_yes(io)? {
        writeln!(io.err, "coda: nothing was posted")?;
        return Ok(());
    }
    if !plan.inline.is_empty() {
        let body = review_payload(&target, &plan);
        send(gh, "POST", &format!("{base}/pulls/{number}/reviews"), &body)?;
        writeln!(
            io.err,
            "coda: pending review created with {} comment(s); submit it on the pull request page",
            plan.inline.len()
        )?;
    }
    if summary_changed {
        let body = json!({ "body": plan.summary });
        let done = if let Some(id) = summary_id {
            send(gh, "PATCH", &format!("{base}/issues/comments/{id}"), &body)?;
            "updated"
        } else {
            send(
                gh,
                "POST",
                &format!("{base}/issues/{number}/comments"),
                &body,
            )?;
            "created"
        };
        writeln!(io.err, "coda: summary comment {done}")?;
    }
    Ok(())
}

/// The pull request as the API names it.
struct Target {
    /// `repos/<owner>/<repo>`.
    repo_path: String,
    number: u64,
    head_sha: String,
}

/// The key the review memory files the pull request under (`owner/repo#number`) and the
/// commit it is at.
///
/// # Errors
/// Returns [`CliError::ReviewPost`] when `gh` fails or its answer lacks a field.
pub fn identify(gh: &mut impl Gh, pr: &str) -> Result<(String, String), CliError> {
    let target = read_target(gh, pr)?;
    let repo = target
        .repo_path
        .strip_prefix("repos/")
        .unwrap_or(&target.repo_path);
    Ok((format!("{repo}#{}", target.number), target.head_sha))
}

/// The commit the pull request is at. Read before the diff, so a push during the review is
/// noticed when posting.
///
/// # Errors
/// Returns [`CliError::ReviewPost`] when `gh` fails or its answer has no head commit.
pub fn head_of(gh: &mut impl Gh, pr: &str) -> Result<String, CliError> {
    read_target(gh, pr).map(|target| target.head_sha)
}

fn read_target(gh: &mut impl Gh, pr: &str) -> Result<Target, CliError> {
    let text = gh.run(&["pr", "view", pr, "--json", PR_FIELDS], None)?;
    let bad = |why: &str| CliError::ReviewPost(format!("could not read the pull request: {why}"));
    let json: Value = serde_json::from_str(&text).map_err(|e| bad(&e.to_string()))?;
    let url = json["url"].as_str().ok_or_else(|| bad("no url"))?;
    let number = json["number"].as_u64().ok_or_else(|| bad("no number"))?;
    let head_sha = json["headRefOid"]
        .as_str()
        .ok_or_else(|| bad("no head commit"))?;
    let (before, _) = url
        .split_once(URL_PULL_MARKER)
        .ok_or_else(|| bad("odd url"))?;
    let segments: Vec<&str> = before.rsplit('/').take(URL_PATH_SEGMENTS).collect();
    let [repo, owner] = segments[..] else {
        return Err(bad("odd url"));
    };
    Ok(Target {
        repo_path: format!("repos/{owner}/{repo}"),
        number,
        head_sha: head_sha.to_owned(),
    })
}

fn login(gh: &mut impl Gh) -> Result<String, CliError> {
    let text = gh.run(&["api", "user"], None)?;
    let json: Value = serde_json::from_str(&text)
        .map_err(|e| CliError::ReviewPost(format!("could not read your gh login: {e}")))?;
    json["login"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| CliError::ReviewPost("could not read your gh login".to_owned()))
}

/// Every item of a paginated list. `gh --paginate` prints one JSON array per page.
fn list(gh: &mut impl Gh, path: &str) -> Result<Vec<Value>, CliError> {
    let text = gh.run(&["api", "--paginate", path], None)?;
    let mut items = Vec::new();
    for page in serde_json::Deserializer::from_str(&text).into_iter::<Vec<Value>>() {
        let page = page.map_err(|e| CliError::ReviewPost(format!("could not read {path}: {e}")))?;
        items.extend(page);
    }
    Ok(items)
}

fn send(gh: &mut impl Gh, method: &str, path: &str, body: &Value) -> Result<(), CliError> {
    let args = ["api", "-X", method, path, "--input", "-"];
    gh.run(&args, Some(&body.to_string()))?;
    Ok(())
}

/// The body of the create-review call. It has no `event`, which is what keeps the review
/// pending.
fn review_payload(target: &Target, plan: &PostPlan) -> Value {
    let comments: Vec<Value> = plan.inline.iter().map(comment_json).collect();
    json!({ "commit_id": target.head_sha, "comments": comments })
}

fn comment_json(c: &InlineComment) -> Value {
    let mut out = json!({
        "path": c.path,
        "line": c.line,
        "side": RIGHT_SIDE,
        "body": c.body,
    });
    if c.start_line < c.line {
        out["start_line"] = json!(c.start_line);
        out["start_side"] = json!(RIGHT_SIDE);
    }
    out
}

fn asked_yes(io: &mut Io<'_>) -> Result<bool, CliError> {
    write!(io.err, "Post this to the pull request? [y/N] ")?;
    io.err.flush()?;
    let mut answer = String::new();
    io.input.read_line(&mut answer)?;
    Ok(YES_ANSWERS.contains(&answer.trim().to_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use coda_review::schema::{
        Category, Confidence, Coverage, Finding, Risk, RunDetails, Severity,
    };

    use super::*;

    const ME: &str = "me";
    const PR_URL: &str = "https://github.com/acme/widgets/pull/7";
    const SHA: &str = "abc123";

    fn finding(line: u32, title: &str) -> Finding {
        Finding {
            id: "F1".to_owned(),
            file: "a.rs".to_owned(),
            start_line: line,
            end_line: line,
            severity: Severity::High,
            category: Category::Bug,
            title: title.to_owned(),
            explanation: "because".to_owned(),
            suggested_patch: None,
            confidence: Confidence::High,
            score: None,
            verifier_reason: None,
            unverified: false,
        }
    }

    fn review(findings: Vec<Finding>) -> Review {
        Review {
            summary: "Adds things.".to_owned(),
            effort: 2,
            risk: Risk::Medium,
            findings,
            compliance: vec![],
            coverage: Coverage::default(),
            run: RunDetails::default(),
        }
    }

    fn scope() -> DiffScope {
        let mut scope = DiffScope::new();
        scope.add("a.rs", 10, 20);
        scope
    }

    /// What the fake `gh` already holds for the pull request.
    #[derive(Default)]
    struct Remote {
        inline_bodies: Vec<String>,
        pending_review: Option<(u64, Vec<String>)>,
        summary: Option<(u64, String)>,
        foreign_summary: bool,
    }

    /// Answers like `gh`, and records the calls that write.
    struct Fake {
        remote: Remote,
        writes: Vec<(Vec<String>, Option<String>)>,
        reads: Vec<String>,
    }

    impl Fake {
        fn new(remote: Remote) -> Self {
            Self {
                remote,
                writes: Vec::new(),
                reads: Vec::new(),
            }
        }
    }

    impl Gh for Fake {
        fn run(&mut self, args: &[&str], stdin: Option<&str>) -> Result<String, CliError> {
            let line = args.join(" ");
            let comment = |id: u64, login: &str, body: &str| json!({"id": id, "body": body, "user": {"login": login}});
            if line.starts_with("pr view") {
                return Ok(json!({"number": 7, "url": PR_URL, "headRefOid": SHA}).to_string());
            }
            if line == "api user" {
                return Ok(json!({"login": ME}).to_string());
            }
            if args.contains(&"POST") || args.contains(&"PATCH") {
                self.writes.push((
                    args.iter().map(|a| (*a).to_owned()).collect(),
                    stdin.map(str::to_owned),
                ));
                return Ok("{}".to_owned());
            }
            self.reads.push(line.clone());
            let items: Vec<Value> = match args.last().copied().unwrap_or_default() {
                "repos/acme/widgets/pulls/7/comments" => self
                    .remote
                    .inline_bodies
                    .iter()
                    .enumerate()
                    .map(|(n, b)| comment(n as u64 + 100, ME, b))
                    .collect(),
                "repos/acme/widgets/pulls/7/reviews" => self
                    .remote
                    .pending_review
                    .iter()
                    .map(|(id, _)| json!({"id": id, "state": PENDING, "user": {"login": ME}}))
                    .collect(),
                "repos/acme/widgets/pulls/7/reviews/55/comments" => self
                    .remote
                    .pending_review
                    .iter()
                    .flat_map(|(_, bodies)| bodies.iter().map(|b| comment(1, ME, b)))
                    .collect(),
                "repos/acme/widgets/issues/7/comments" => {
                    let mut all = Vec::new();
                    if self.remote.foreign_summary {
                        all.push(comment(8, "someone", SUMMARY_MARKER));
                    }
                    if let Some((id, body)) = &self.remote.summary {
                        all.push(comment(*id, ME, body));
                    }
                    all
                }
                other => panic!("unexpected gh call: {other}"),
            };
            Ok(serde_json::to_string(&items).unwrap())
        }
    }

    fn run_post(
        fake: &mut Fake,
        confirm: Confirm,
        answer: &str,
        review: &Review,
    ) -> (Result<(), CliError>, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut input = Cursor::new(answer.as_bytes().to_vec());
        let result = {
            let mut io = Io {
                out: &mut out,
                err: &mut err,
                input: &mut input,
            };
            post(fake, &mut io, confirm, "7", SHA, review, &[&scope()])
        };
        (result, String::from_utf8(err).unwrap())
    }

    const YES: Confirm = Confirm {
        assume_yes: true,
        interactive: false,
    };

    fn payload(write: &(Vec<String>, Option<String>)) -> Value {
        serde_json::from_str(write.1.as_deref().unwrap()).unwrap()
    }

    #[test]
    fn a_first_post_creates_a_pending_review_and_a_summary() {
        // Arrange
        let mut fake = Fake::new(Remote::default());
        let review = review(vec![finding(12, "Off by one"), finding(99, "Far away")]);

        // Act
        let (result, _) = run_post(&mut fake, YES, "", &review);

        // Assert
        result.unwrap();
        assert_eq!(fake.writes.len(), 2);
        let (args, _) = &fake.writes[0];
        assert!(
            args.join(" ")
                .contains("POST repos/acme/widgets/pulls/7/reviews")
        );
        let body = payload(&fake.writes[0]);
        assert_eq!(body["commit_id"], SHA);
        assert!(body.get("event").is_none(), "the review must stay pending");
        assert_eq!(body["comments"].as_array().unwrap().len(), 1);
        assert_eq!(body["comments"][0]["path"], "a.rs");
        assert_eq!(body["comments"][0]["line"], 12);
        assert_eq!(body["comments"][0]["side"], RIGHT_SIDE);
        assert!(body["comments"][0].get("start_line").is_none());
        let (args, _) = &fake.writes[1];
        assert!(
            args.join(" ")
                .contains("POST repos/acme/widgets/issues/7/comments")
        );
        let summary = payload(&fake.writes[1])["body"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(summary.starts_with(SUMMARY_MARKER));
        assert!(summary.contains("Far away"));
    }

    #[test]
    fn a_pull_request_that_moved_since_the_review_began_is_refused() {
        // Arrange
        let mut fake = Fake::new(Remote::default());
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut input = Cursor::new(Vec::new());
        let mut io = Io {
            out: &mut out,
            err: &mut err,
            input: &mut input,
        };
        let review = review(vec![finding(12, "Off by one")]);

        // Act
        let result = post(&mut fake, &mut io, YES, "7", "older", &review, &[&scope()]);

        // Assert
        assert!(matches!(result, Err(CliError::ReviewPost(m)) if m.contains("changed")));
        assert_eq!(fake.writes.len(), 0);
    }

    #[test]
    fn a_range_comment_carries_its_first_line() {
        // Arrange
        let mut fake = Fake::new(Remote::default());
        let mut f = finding(12, "Range");
        f.end_line = 14;

        // Act
        run_post(&mut fake, YES, "", &review(vec![f])).0.unwrap();

        // Assert
        let comment = &payload(&fake.writes[0])["comments"][0];
        assert_eq!(comment["start_line"], 12);
        assert_eq!(comment["line"], 14);
        assert_eq!(comment["start_side"], RIGHT_SIDE);
    }

    #[test]
    fn a_rerun_edits_the_summary_and_skips_comments_already_posted() {
        // Arrange
        let posted = plan(
            &review(vec![finding(12, "Off by one")]),
            &[&scope()],
            &BTreeSet::new(),
        );
        let mut fake = Fake::new(Remote {
            inline_bodies: vec![posted.inline[0].body.clone()],
            summary: Some((41, format!("{SUMMARY_MARKER}\nold summary"))),
            ..Remote::default()
        });
        let review = review(vec![finding(12, "off by one!")]);

        // Act
        let (result, _) = run_post(&mut fake, YES, "", &review);

        // Assert
        result.unwrap();
        assert_eq!(fake.writes.len(), 1, "no new review, only the summary edit");
        assert!(
            fake.writes[0]
                .0
                .join(" ")
                .contains("PATCH repos/acme/widgets/issues/comments/41")
        );
    }

    #[test]
    fn a_comment_in_the_users_pending_review_counts_as_posted() {
        // Arrange
        let posted = plan(
            &review(vec![finding(12, "Off by one")]),
            &[&scope()],
            &BTreeSet::new(),
        );
        let mut fake = Fake::new(Remote {
            pending_review: Some((55, vec![posted.inline[0].body.clone()])),
            ..Remote::default()
        });

        // Act
        let (result, _) = run_post(&mut fake, YES, "", &review(vec![finding(12, "Off by one")]));

        // Assert
        result.unwrap();
        assert!(
            fake.writes
                .iter()
                .all(|w| !w.0.join(" ").contains("pulls/7/reviews"))
        );
    }

    #[test]
    fn a_new_comment_is_refused_while_the_user_has_a_pending_review() {
        // Arrange
        let mut fake = Fake::new(Remote {
            pending_review: Some((55, Vec::new())),
            ..Remote::default()
        });

        // Act
        let (result, _) = run_post(&mut fake, YES, "", &review(vec![finding(12, "New")]));

        // Assert
        assert!(matches!(result, Err(CliError::ReviewPost(m)) if m.contains("pending review")));
        assert_eq!(fake.writes.len(), 0);
    }

    #[test]
    fn a_summary_with_the_marker_from_someone_else_is_not_edited() {
        // Arrange
        let mut fake = Fake::new(Remote {
            foreign_summary: true,
            ..Remote::default()
        });

        // Act
        run_post(&mut fake, YES, "", &review(vec![finding(99, "Far")]))
            .0
            .unwrap();

        // Assert
        assert!(
            fake.writes[0]
                .0
                .join(" ")
                .contains("POST repos/acme/widgets/issues/7/comments")
        );
    }

    #[test]
    fn declining_the_preview_writes_nothing() {
        // Arrange
        let mut fake = Fake::new(Remote::default());
        let ask = Confirm {
            assume_yes: false,
            interactive: true,
        };

        // Act
        let (result, err) = run_post(
            &mut fake,
            ask,
            "n\n",
            &review(vec![finding(12, "Off by one")]),
        );

        // Assert
        result.unwrap();
        assert_eq!(fake.writes.len(), 0);
        assert!(err.contains("a.rs:12 Off by one"), "{err}");
        assert!(err.contains("[y/N]"), "{err}");
    }

    #[test]
    fn answering_yes_posts() {
        // Arrange
        let mut fake = Fake::new(Remote::default());
        let ask = Confirm {
            assume_yes: false,
            interactive: true,
        };

        // Act
        run_post(
            &mut fake,
            ask,
            "y\n",
            &review(vec![finding(12, "Off by one")]),
        )
        .0
        .unwrap();

        // Assert
        assert_eq!(fake.writes.len(), 2);
    }

    #[test]
    fn without_yes_and_without_a_terminal_the_run_is_refused() {
        // Arrange
        let confirm = Confirm {
            assume_yes: false,
            interactive: false,
        };

        // Act and Assert
        assert!(matches!(confirm.check(), Err(CliError::ReviewPost(m)) if m.contains("--yes")));
        assert!(YES.check().is_ok());
    }

    #[test]
    fn a_review_with_nothing_new_and_an_unchanged_summary_posts_nothing() {
        // Arrange
        let review = review(vec![]);
        let summary = plan(&review, &[&scope()], &BTreeSet::new()).summary;
        let mut fake = Fake::new(Remote {
            summary: Some((41, summary)),
            ..Remote::default()
        });

        // Act
        let (result, err) = run_post(&mut fake, YES, "", &review);

        // Assert
        result.unwrap();
        assert_eq!(fake.writes.len(), 0);
        assert!(err.contains("nothing to post"), "{err}");
    }
}
