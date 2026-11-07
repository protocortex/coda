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

use crate::pr_watch::{HeadKind, PrReader, PrState, Snapshot};

const GH_LIMIT: Duration = Duration::from_secs(60);
const VIEW_FIELDS: &str = "state,mergeable,comments,reviews,headRefOid,isDraft";
const BOT_ACCOUNT_TYPE: &str = "Bot";
const BOT_LOGIN_SUFFIX: &str = "[bot]";
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
    /// The newest commit looked at and what it is, so a commit that could be read is read once.
    head: Mutex<Option<(String, HeadKind)>>,
}

impl GhReader {
    pub(crate) fn new(gh: OsString, cwd: PathBuf, pr: String) -> Self {
        Self {
            gh,
            cwd,
            pr,
            login: Mutex::new(None),
            head: Mutex::new(None),
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

    fn head_kind(&self, head: &str) -> HeadKind {
        let mut seen = self
            .head
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((known, kind)) = seen.as_ref()
            && known == head
        {
            return *kind;
        }
        let endpoint = format!("repos/{{owner}}/{{repo}}/commits/{head}");
        let kind = match self.gh(&["api", &endpoint]) {
            Ok((out, _, true)) => parse_commit(&out),
            _ => HeadKind::Unknown,
        };
        if kind != HeadKind::Unknown {
            *seen = Some((head.to_owned(), kind));
        }
        kind
    }

    fn read_now(&self) -> Result<Snapshot, String> {
        let login = self.own_login()?;
        let (out, err, ok) = self.gh(&["pr", "view", self.pr.as_str(), "--json", VIEW_FIELDS])?;
        if !ok {
            return Err(format!("gh pr view failed: {}", err.trim()));
        }
        let view = parse_view(&out, &login)?;
        let head_kind = if view.head.is_empty() {
            HeadKind::Unknown
        } else {
            self.head_kind(&view.head)
        };
        let mut snapshot = Snapshot {
            state: view.state,
            conflicting: view.conflicting,
            failed_checks: Vec::new(),
            required_passed: false,
            others_comments: view.others,
            head: view.head,
            draft: view.draft,
            head_kind,
        };
        if snapshot.state != PrState::Open {
            return Ok(snapshot);
        }
        let all = self.checks(false)?;
        let required = self.checks(true)?;
        snapshot.failed_checks = failed(&all);
        snapshot.required_passed = !required.is_empty()
            && required
                .iter()
                .all(|(_, bucket)| bucket == BUCKET_PASS || bucket == BUCKET_SKIP);
        Ok(snapshot)
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

/// What `gh pr view --json` says about a pull request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct View {
    pub(crate) state: PrState,
    pub(crate) conflicting: bool,
    /// Identifiers of the comments and reviews not written by the watching user.
    pub(crate) others: Vec<String>,
    pub(crate) head: String,
    pub(crate) draft: bool,
}

/// Reads the JSON `gh pr view --json` prints, leaving out what `own_login` wrote.
pub(crate) fn parse_view(json: &str, own_login: &str) -> Result<View, String> {
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
    Ok(View {
        state,
        conflicting,
        others,
        head: view
            .get("headRefOid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        draft: view
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// What a commit is, from the JSON the commits endpoint prints.
pub(crate) fn parse_commit(json: &str) -> HeadKind {
    let Ok(commit) = serde_json::from_str::<Value>(json) else {
        return HeadKind::Unknown;
    };
    let Some(parents) = commit.get("parents").and_then(Value::as_array) else {
        return HeadKind::Unknown;
    };
    if parents.len() > 1 {
        return HeadKind::Merge;
    }
    let by_bot = commit.pointer("/author/type").and_then(Value::as_str) == Some(BOT_ACCOUNT_TYPE)
        || commit
            .pointer("/author/login")
            .and_then(Value::as_str)
            .is_some_and(|login| login.ends_with(BOT_LOGIN_SUFFIX));
    if by_bot {
        HeadKind::Bot
    } else {
        HeadKind::Normal
    }
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
        "headRefOid": "abc",
        "isDraft": true,
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
        let view = parse_view(VIEW, "me").unwrap();

        // Assert
        assert_eq!(view.state, PrState::Open);
        assert!(view.conflicting);
        assert_eq!(view.others, ["c2", "r1"]);
        assert_eq!(view.head, "abc");
        assert!(view.draft);
    }

    #[test]
    fn merged_and_closed_states_are_read() {
        // Arrange
        let merged = r#"{"state":"MERGED","mergeable":"UNKNOWN"}"#;
        let closed = r#"{"state":"CLOSED"}"#;

        // Act and Assert
        assert_eq!(parse_view(merged, "me").unwrap().state, PrState::Merged);
        assert_eq!(parse_view(closed, "me").unwrap().state, PrState::Closed);
    }

    #[test]
    fn a_commit_is_a_merge_a_bot_a_person_or_unknown() {
        // Arrange
        let merge = r#"{"parents":[{"sha":"1"},{"sha":"2"}],"author":{"login":"a","type":"User"}}"#;
        let bot = r#"{"parents":[{"sha":"1"}],"author":{"login":"x","type":"Bot"}}"#;
        let bot_login =
            r#"{"parents":[{"sha":"1"}],"author":{"login":"dependabot[bot]","type":"User"}}"#;
        let person = r#"{"parents":[{"sha":"1"}],"author":{"login":"a","type":"User"}}"#;

        // Act and Assert
        assert_eq!(parse_commit(merge), HeadKind::Merge);
        assert_eq!(parse_commit(bot), HeadKind::Bot);
        assert_eq!(parse_commit(bot_login), HeadKind::Bot);
        assert_eq!(parse_commit(person), HeadKind::Normal);
        assert_eq!(parse_commit("{}"), HeadKind::Unknown);
        assert_eq!(parse_commit("nope"), HeadKind::Unknown);
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
