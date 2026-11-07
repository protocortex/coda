// SPDX-License-Identifier: Apache-2.0

//! Reads the issues a pull request links to, through `gh issue view`, for `coda review`. An
//! issue that cannot be read is skipped with a note and never guessed.

use coda_review::issues::{
    GH_ISSUE_FIELDS, GH_PR_TEXT_FIELDS, LinkedIssue, issue_from_gh, parse_refs, pr_text_from_gh,
    select,
};

use crate::error::CliError;
use crate::review_post::Gh;

/// The issues a review reads, and a note for each one it could not.
#[derive(Debug, Default)]
pub struct Linked {
    /// The issues that were read.
    pub issues: Vec<LinkedIssue>,
    /// Why each skipped issue was skipped.
    pub notes: Vec<String>,
}

fn reason(error: CliError) -> String {
    match error {
        CliError::ReviewPost(why) => why,
        other => other.to_string(),
    }
}

/// Finds the issues named by the title, body and branch of the pull request `pr`, and reads
/// up to three of them. Issues of another repository are read only when it is in `allowed`.
/// Nothing here fails the review: a pull request or an issue that cannot be read becomes a
/// note.
pub fn gather(gh: &mut impl Gh, pr: &str, allowed: &[String]) -> Linked {
    let mut linked = Linked::default();
    let text = match gh
        .run(&["pr", "view", pr, "--json", GH_PR_TEXT_FIELDS], None)
        .map_err(reason)
        .and_then(|json| pr_text_from_gh(&json).map_err(|e| e.to_string()))
    {
        Ok(text) => text,
        Err(why) => {
            linked.notes.push(format!(
                "linked issues skipped: the pull request text could not be read: {why}"
            ));
            return linked;
        }
    };
    let refs = parse_refs(&text.title, &text.body, &text.branch);
    let (chosen, refused) = select(refs, &text.repo, allowed);
    linked.notes.extend(refused);
    for reference in chosen {
        let label = reference.label();
        let repo = reference.repo.clone().unwrap_or_else(|| text.repo.clone());
        let number = reference.number.to_string();
        let args = [
            "issue",
            "view",
            number.as_str(),
            "--repo",
            repo.as_str(),
            "--json",
            GH_ISSUE_FIELDS,
        ];
        let read = gh
            .run(&args, None)
            .map_err(reason)
            .and_then(|json| issue_from_gh(reference, &json).map_err(|e| e.to_string()));
        match read {
            Ok(issue) => linked.issues.push(issue),
            Err(why) => linked.notes.push(format!("{label} skipped: {why}")),
        }
    }
    linked
}

#[cfg(test)]
mod tests {
    use super::*;

    const PR: &str = r#"{"title":"Fix #12","body":"Also acme/other#3, evil/x#4 and #99.","headRefName":"7-thing","url":"https://github.com/acme/widgets/pull/5"}"#;
    const ISSUE: &str = r#"{"title":"Add retries","body":"- [ ] Retry three times","labels":[]}"#;

    /// A `gh` that knows the pull request and issues 12 and 3 and not 99.
    fn fake(
        calls: &mut Vec<String>,
    ) -> impl FnMut(&[&str], Option<&str>) -> Result<String, CliError> + '_ {
        move |args, _| {
            calls.push(args.join(" "));
            match args {
                ["pr", "view", ..] => Ok(PR.to_owned()),
                ["issue", "view", "99", ..] => Err(CliError::ReviewPost(
                    "`gh` failed: Could not resolve to an issue".to_owned(),
                )),
                ["issue", "view", ..] => Ok(ISSUE.to_owned()),
                other => panic!("unexpected call {other:?}"),
            }
        }
    }

    #[test]
    fn issues_of_the_repo_and_of_listed_repos_are_read_and_others_are_refused() {
        // Arrange
        let mut calls = Vec::new();

        // Act
        let linked = gather(&mut fake(&mut calls), "5", &["acme/other".to_owned()]);

        // Assert
        let labels: Vec<String> = linked.issues.iter().map(|i| i.reference.label()).collect();
        assert_eq!(labels, ["#12", "acme/other#3"]);
        assert!(
            calls
                .contains(&"issue view 12 --repo acme/widgets --json title,body,labels".to_owned())
        );
        assert!(
            calls.contains(&"issue view 3 --repo acme/other --json title,body,labels".to_owned())
        );
        assert!(
            linked
                .notes
                .iter()
                .any(|n| n.contains("evil/x#4") && n.contains("linked_repos"))
        );
    }

    #[test]
    fn an_issue_that_cannot_be_read_is_a_note_and_not_an_issue() {
        // Arrange
        let mut calls = Vec::new();

        // Act
        let linked = gather(&mut fake(&mut calls), "5", &["acme/other".to_owned()]);

        // Assert
        assert!(linked.issues.iter().all(|i| i.reference.number != 99));
        assert!(
            linked
                .notes
                .iter()
                .any(|n| n.starts_with("#99 skipped:") && n.contains("Could not resolve"))
        );
    }

    #[test]
    fn a_pull_request_that_cannot_be_read_gives_a_note_and_no_issues() {
        // Arrange
        let mut gh = |_: &[&str], _: Option<&str>| Err(CliError::ReviewPost("offline".to_owned()));

        // Act
        let linked = gather(&mut gh, "5", &[]);

        // Assert
        assert_eq!(linked.issues.len(), 0);
        assert_eq!(linked.notes.len(), 1);
        assert!(linked.notes[0].contains("offline"));
    }

    #[test]
    fn a_pull_request_with_no_references_gives_neither_issues_nor_notes() {
        // Arrange
        let mut gh = |_: &[&str], _: Option<&str>| {
            Ok(r#"{"title":"Tidy","body":"","headRefName":"tidy","url":"https://github.com/a/b/pull/1"}"#.to_owned())
        };

        // Act
        let linked = gather(&mut gh, "1", &[]);

        // Assert
        assert_eq!(linked.issues.len(), 0);
        assert_eq!(linked.notes.len(), 0);
    }
}
