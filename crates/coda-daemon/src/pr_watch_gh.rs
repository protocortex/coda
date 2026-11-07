// SPDX-License-Identifier: Apache-2.0

//! Reads a pull request for PR watch with the user's own `gh`.
//!
//! Every call goes through the user's `gh` login. Nothing here reads a token or
//! a credential file.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use coda_selfhost::process::run_bounded;
use serde_json::Value;

use crate::pr_watch::{PrReader, PrState, Snapshot};

const GH_LIMIT: Duration = Duration::from_secs(60);
const VIEW_FIELDS: &str = "state,mergeable,comments,reviews";
const CHECK_FIELDS: &str = "name,bucket";
pub(crate) const URL_PREFIX: &str = "https://";
const BUCKET_FAIL: &str = "fail";
const BUCKET_PASS: &str = "pass";
const BUCKET_SKIP: &str = "skipping";
const STATE_MERGED: &str = "MERGED";
const STATE_CLOSED: &str = "CLOSED";
const MERGEABLE_CONFLICT: &str = "CONFLICTING";
const REVIEW_PENDING: &str = "PENDING";
/// What `gh pr checks` says on stderr when the pull request has no checks of that kind.
const NO_CHECKS_TEXTS: [&str; 2] = ["no checks reported", "no required checks reported"];

/// Reads one pull request, in the folder of the session that watches it.
pub(crate) struct GhReader {
    gh: OsString,
    cwd: PathBuf,
    pr: String,
    login: Mutex<Option<String>>,
}

impl GhReader {
    pub(crate) fn new(gh: OsString, cwd: PathBuf, pr: String) -> Self {
        Self {
            gh,
            cwd,
            pr,
            login: Mutex::new(None),
        }
    }

    fn gh(&self, args: &[&str]) -> Result<(String, String, bool), String> {
        let mut command = Command::new(&self.gh);
        command
            .args(args)
            .current_dir(&self.cwd)
            .env("GH_PROMPT_DISABLED", "1");
        let done = run_bounded(&mut command, GH_LIMIT).map_err(|e| format!("gh: {e}"))?;
        Ok((done.stdout, done.stderr, done.status.success()))
    }

    fn own_login(&self) -> Result<String, String> {
        let mut held = self
            .login
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(login) = held.as_ref() {
            return Ok(login.clone());
        }
        let (out, err, ok) = self.gh(&["api", "user"])?;
        if !ok {
            return Err(format!("gh api user failed: {}", err.trim()));
        }
        let login = serde_json::from_str::<Value>(&out)
            .ok()
            .and_then(|user| user.get("login")?.as_str().map(str::to_owned))
            .ok_or_else(|| "gh api user gave no login".to_owned())?;
        *held = Some(login.clone());
        Ok(login)
    }

    fn checks(&self, required: bool) -> Result<Vec<(String, String)>, String> {
        let mut args = vec!["pr", "checks", self.pr.as_str(), "--json", CHECK_FIELDS];
        if required {
            args.push("--required");
        }
        let (out, err, _) = self.gh(&args)?;
        match parse_checks(&out) {
            Some(checks) => Ok(checks),
            None if out.trim().is_empty()
                && NO_CHECKS_TEXTS.iter().any(|text| err.contains(text)) =>
            {
                Ok(Vec::new())
            }
            None => Err(format!("gh pr checks failed: {}", err.trim())),
        }
    }

    fn read_now(&self) -> Result<Snapshot, String> {
        let login = self.own_login()?;
        let (out, err, ok) = self.gh(&["pr", "view", self.pr.as_str(), "--json", VIEW_FIELDS])?;
        if !ok {
            return Err(format!("gh pr view failed: {}", err.trim()));
        }
        let (state, conflicting, others_comments) = parse_view(&out, &login)?;
        if state != PrState::Open {
            return Ok(Snapshot {
                state,
                conflicting,
                failed_checks: Vec::new(),
                required_passed: false,
                others_comments,
            });
        }
        let all = self.checks(false)?;
        let required = self.checks(true)?;
        Ok(Snapshot {
            state,
            conflicting,
            failed_checks: failed(&all),
            required_passed: !required.is_empty()
                && required
                    .iter()
                    .all(|(_, bucket)| bucket == BUCKET_PASS || bucket == BUCKET_SKIP),
            others_comments,
        })
    }
}

impl PrReader for std::sync::Arc<GhReader> {
    async fn read(&self) -> Result<Snapshot, String> {
        let reader = std::sync::Arc::clone(self);
        tokio::task::spawn_blocking(move || reader.read_now())
            .await
            .map_err(|e| format!("the read did not finish: {e}"))?
    }
}

fn failed(checks: &[(String, String)]) -> Vec<String> {
    checks
        .iter()
        .filter(|(_, bucket)| bucket == BUCKET_FAIL)
        .map(|(name, _)| name.clone())
        .collect()
}

/// The `(name, bucket)` of each check in the JSON `gh pr checks --json` prints.
pub(crate) fn parse_checks(json: &str) -> Option<Vec<(String, String)>> {
    let list: Vec<Value> = serde_json::from_str(json).ok()?;
    Some(
        list.iter()
            .filter_map(|check| {
                Some((
                    check.get("name")?.as_str()?.to_owned(),
                    check.get("bucket")?.as_str()?.to_owned(),
                ))
            })
            .collect(),
    )
}

/// The state, whether it conflicts, and the identifiers of the comments and
/// reviews not written by `own_login`, from the JSON `gh pr view --json` prints.
pub(crate) fn parse_view(
    json: &str,
    own_login: &str,
) -> Result<(PrState, bool, Vec<String>), String> {
    let view: Value = serde_json::from_str(json).map_err(|e| format!("gh pr view output: {e}"))?;
    let state = match view.get("state").and_then(Value::as_str) {
        Some(STATE_MERGED) => PrState::Merged,
        Some(STATE_CLOSED) => PrState::Closed,
        _ => PrState::Open,
    };
    let conflicting = view.get("mergeable").and_then(Value::as_str) == Some(MERGEABLE_CONFLICT);
    let by_others = |key: &str, skip_state: Option<&str>| -> Vec<String> {
        view.get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| {
                let author = item.pointer("/author/login").and_then(Value::as_str);
                author != Some(own_login)
                    && skip_state
                        .is_none_or(|skip| item.get("state").and_then(Value::as_str) != Some(skip))
            })
            .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_owned))
            .collect()
    };
    let mut others = by_others("comments", None);
    others.extend(by_others("reviews", Some(REVIEW_PENDING)));
    Ok((state, conflicting, others))
}

/// Whether `pr` names a pull request by number or by address, so it cannot be read as a flag.
pub(crate) fn is_pull_request_reference(pr: &str) -> bool {
    let number = !pr.is_empty() && pr.bytes().all(|b| b.is_ascii_digit());
    let address = pr.starts_with(URL_PREFIX)
        && pr.len() <= crate::pr_watch::MAX_REFERENCE_BYTES
        && !pr.chars().any(|c| c.is_control() || c.is_whitespace());
    number || address
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIEW: &str = r#"{
        "state": "OPEN",
        "mergeable": "CONFLICTING",
        "comments": [
            {"id": "c1", "author": {"login": "me"}},
            {"id": "c2", "author": {"login": "other"}}
        ],
        "reviews": [
            {"id": "r1", "author": {"login": "other"}, "state": "COMMENTED"},
            {"id": "r2", "author": {"login": "other"}, "state": "PENDING"},
            {"id": "r3", "author": {"login": "me"}, "state": "APPROVED"}
        ]
    }"#;

    #[test]
    fn a_view_gives_the_state_the_conflict_and_only_what_others_wrote() {
        // Arrange and Act
        let (state, conflicting, others) = parse_view(VIEW, "me").unwrap();

        // Assert
        assert_eq!(state, PrState::Open);
        assert!(conflicting);
        assert_eq!(others, ["c2", "r1"]);
    }

    #[test]
    fn merged_and_closed_states_are_read() {
        // Arrange
        let merged = r#"{"state":"MERGED","mergeable":"UNKNOWN"}"#;
        let closed = r#"{"state":"CLOSED"}"#;

        // Act and Assert
        assert_eq!(parse_view(merged, "me").unwrap().0, PrState::Merged);
        assert_eq!(parse_view(closed, "me").unwrap().0, PrState::Closed);
    }

    #[test]
    fn output_that_is_not_json_is_an_error() {
        // Arrange and Act
        let read = parse_view("not json", "me");

        // Assert
        assert!(read.is_err());
    }

    #[test]
    fn checks_keep_their_names_and_buckets() {
        // Arrange
        let json =
            r#"[{"name":"build","bucket":"fail"},{"name":"lint","bucket":"pass"},{"bucket":"x"}]"#;

        // Act
        let checks = parse_checks(json).unwrap();

        // Assert
        assert_eq!(
            checks,
            [
                ("build".to_owned(), "fail".to_owned()),
                ("lint".to_owned(), "pass".to_owned())
            ]
        );
        assert_eq!(failed(&checks), ["build"]);
        assert_eq!(parse_checks("no checks reported"), None);
    }

    #[test]
    fn only_a_number_or_a_web_address_names_a_pull_request() {
        // Arrange
        let good = ["7", "https://github.com/o/r/pull/7"];
        let bad = [
            "",
            "--web",
            "main",
            "7 --web",
            "http://github.com/o/r/pull/7",
            "https://x/\u{1b}",
        ];

        // Act and Assert
        for pr in good {
            assert!(is_pull_request_reference(pr), "{pr:?}");
        }
        for pr in bad {
            assert!(!is_pull_request_reference(pr), "{pr:?}");
        }
    }
}
