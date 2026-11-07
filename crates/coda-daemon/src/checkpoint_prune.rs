// SPDX-License-Identifier: Apache-2.0

//! Removing the git refs that keep old checkpoints alive.
//!
//! Every checkpoint is a commit that only a ref under
//! `refs/coda/checkpoints/<session>/` holds, so its files stay in the object
//! store until the ref goes. Pruning removes refs and nothing else: it does not
//! run `git gc`, which collects what the refs held on its own schedule. Only
//! the refs of sessions this daemon knows are looked at, and the `base` ref and
//! the newest turn of each session always stay.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use coda_config::checkpoints::PruneConfig;
use coda_protocol::methods::{CheckpointsPruneResult, PrunedRef, PrunedSession};
use coda_protocol::types::SessionId;

use crate::checkpoint::ref_prefix;
use crate::worktree::git;

const SECONDS_PER_DAY: i64 = 86_400;
const REWIND_PREFIX: &str = "pre-rewind-";

/// One session to look at: where its repository is.
pub(crate) struct Subject {
    pub(crate) session: SessionId,
    /// A folder inside the repository, or the repository itself.
    pub(crate) folder: PathBuf,
    /// The refs this database recorded on the session's turns. Another
    /// database that shares the repository can hold refs under the same
    /// session number, and those are never touched.
    pub(crate) recorded: HashSet<String>,
    /// The refs of turns a rewind rolled back.
    pub(crate) rolled_back: HashSet<String>,
}

/// What pruning needs from the world, so a test can stand in for it.
pub(crate) struct Probes<'a> {
    pub(crate) now_secs: i64,
    /// Whether a turn is running in the session.
    pub(crate) running: &'a dyn Fn(&SessionId) -> bool,
}

/// Works out which refs of each session go and, when `apply` is set, removes
/// them. A removal that fails is reported, not raised.
pub(crate) fn prune(
    subjects: &[Subject],
    policy: &PruneConfig,
    probes: &Probes<'_>,
    apply: bool,
) -> CheckpointsPruneResult {
    let sessions = subjects
        .iter()
        .filter_map(|subject| look_at(subject, policy, probes, apply))
        .collect();
    CheckpointsPruneResult {
        applied: apply,
        sessions,
    }
}

/// A checkpoint ref as git lists it.
struct Found {
    reference: String,
    label: String,
    /// Commit time in seconds since the Unix epoch.
    at: i64,
}

fn look_at(
    subject: &Subject,
    policy: &PruneConfig,
    probes: &Probes<'_>,
    apply: bool,
) -> Option<PrunedSession> {
    let session = subject.session.clone();
    let note = |why: &str, kept| PrunedSession {
        session_id: session.clone(),
        kept,
        refs: Vec::new(),
        bytes: 0,
        note: Some(why.to_owned()),
    };
    if !subject.folder.exists() {
        return None;
    }
    let found = list(&subject.session, &subject.folder)
        .ok()
        .filter(|found| !found.is_empty())?;
    let total = u64::try_from(found.len()).unwrap_or(u64::MAX);
    if (probes.running)(&subject.session) {
        return Some(note("a turn is running", total));
    }
    let mut refs = decide(&found, subject, policy, probes.now_secs);
    let bytes = freed_bytes(&subject.folder, &refs);
    if apply {
        for pruned in &mut refs {
            pruned.removed = git(&subject.folder, &["update-ref", "-d", &pruned.reference]).is_ok();
        }
    }
    let gone = refs.iter().filter(|r| r.removed || !apply).count();
    Some(PrunedSession {
        session_id: session,
        kept: total - u64::try_from(gone).unwrap_or(0),
        refs,
        bytes,
        note: None,
    })
}

/// The refs of `session`, read from the repository that holds `folder`.
fn list(session: &SessionId, folder: &Path) -> Result<Vec<Found>, crate::worktree::WorktreeError> {
    let prefix = ref_prefix(session.as_str());
    let listing = git(
        folder,
        &[
            "for-each-ref",
            "--format=%(refname) %(committerdate:unix)",
            &prefix,
        ],
    )?;
    Ok(listing
        .lines()
        .filter_map(|line| {
            let (reference, at) = line.rsplit_once(' ')?;
            Some(Found {
                label: reference.strip_prefix(&prefix)?.to_owned(),
                reference: reference.to_owned(),
                at: at.parse().ok()?,
            })
        })
        .collect())
}

/// The refs that go. Only refs this database recorded are turn refs here. The
/// `base` ref and the newest live turn stay; live turns beyond the newest
/// `keep_turns` or older than `keep_days` go; a turn a rewind rolled back goes
/// after `keep_days`. A rewind safety save goes after `rewind_days`, but only
/// when every other ref under the session number is recorded here, since a
/// save is not recorded and could belong to another database. A ref with any
/// other name stays.
fn decide(
    found: &[Found],
    subject: &Subject,
    policy: &PruneConfig,
    now_secs: i64,
) -> Vec<PrunedRef> {
    let age_days = |at: i64| now_secs.saturating_sub(at) / SECONDS_PER_DAY;
    let old = |at: i64, days: u32| age_days(at) >= i64::from(days);
    let is_save = |f: &Found| f.label.starts_with(REWIND_PREFIX);
    let mut gone = Vec::new();
    let mut push = |found: &Found, reason: String| {
        gone.push(PrunedRef {
            reference: found.reference.clone(),
            reason,
            removed: false,
        });
    };
    let mut live: Vec<(u64, &Found)> = Vec::new();
    for f in found {
        let Ok(number) = f.label.parse::<u64>() else {
            continue;
        };
        if !subject.recorded.contains(&f.reference) {
            continue;
        }
        if subject.rolled_back.contains(&f.reference) {
            if old(f.at, policy.keep_days) {
                push(
                    f,
                    format!("a rolled back turn older than {} days", policy.keep_days),
                );
            }
        } else {
            live.push((number, f));
        }
    }
    live.sort_by_key(|(number, _)| *number);
    let keep_turns = usize::try_from(policy.keep_turns).unwrap_or(usize::MAX);
    let beyond = live.len().saturating_sub(keep_turns);
    for (index, (_, turn)) in live.iter().enumerate() {
        if index + 1 == live.len() {
            continue;
        }
        if index < beyond {
            push(
                turn,
                format!("beyond the newest {} turns", policy.keep_turns),
            );
        } else if old(turn.at, policy.keep_days) {
            push(turn, format!("older than {} days", policy.keep_days));
        }
    }
    let shared = found
        .iter()
        .any(|f| !is_save(f) && !subject.recorded.contains(&f.reference));
    if !shared {
        for save in found
            .iter()
            .filter(|f| is_save(f) && old(f.at, policy.rewind_days))
        {
            push(
                save,
                format!("a rewind save older than {} days", policy.rewind_days),
            );
        }
    }
    gone
}

/// Bytes that only `refs` keep alive: what is reachable from them and from no
/// other ref. An estimate: it does not count objects `git gc` packs together.
fn freed_bytes(folder: &Path, refs: &[PrunedRef]) -> u64 {
    if refs.is_empty() {
        return 0;
    }
    let mut args = vec![
        "rev-list".to_owned(),
        "--objects".to_owned(),
        "--disk-usage".to_owned(),
    ];
    args.extend(refs.iter().map(|r| r.reference.clone()));
    args.push("--not".to_owned());
    args.extend(refs.iter().map(|r| format!("--exclude={}", r.reference)));
    args.push("--all".to_owned());
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git(folder, &args)
        .ok()
        .and_then(|out| out.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::checkpoint::ref_name;
    use std::process::Command;
    use tempfile::TempDir;

    const NOW: i64 = 1_800_000_000;

    fn run(dir: &Path, args: &[&str], date: Option<i64>) -> String {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        if let Some(date) = date {
            command.env("GIT_COMMITTER_DATE", format!("{date} +0000"));
        }
        let out = command.output().unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        run(dir.path(), &["init", "-q", "-b", "main"], None);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        run(dir.path(), &["add", "."], None);
        run(dir.path(), &["commit", "-q", "-m", "first"], None);
        dir
    }

    /// A checkpoint ref on a commit of its own, made `days_ago` days before `NOW`.
    fn checkpoint(dir: &Path, session: &str, label: &str, days_ago: i64) {
        let name = format!("{}{label}", ref_prefix(session));
        std::fs::write(dir.join("a.txt"), format!("{session} {label}\n")).unwrap();
        run(dir, &["add", "."], None);
        let tree = run(dir, &["write-tree"], None);
        let commit = run(
            dir,
            &["commit-tree", tree.trim(), "-m", "checkpoint"],
            Some(NOW - days_ago * SECONDS_PER_DAY),
        );
        run(dir, &["update-ref", &name, commit.trim()], None);
        run(dir, &["reset", "-q", "--hard", "HEAD"], None);
    }

    fn refs_of(dir: &Path, session: &str) -> Vec<String> {
        run(
            dir,
            &["for-each-ref", "--format=%(refname)", &ref_prefix(session)],
            None,
        )
        .lines()
        .map(|r| r.strip_prefix(&ref_prefix(session)).unwrap().to_owned())
        .collect()
    }

    fn policy() -> PruneConfig {
        PruneConfig {
            enabled: true,
            keep_days: 30,
            keep_turns: 3,
            rewind_days: 7,
        }
    }

    /// A subject that has recorded every ref the session has now, except `save`s.
    fn subject(dir: &Path, session: &str) -> Subject {
        let recorded = refs_of(dir, session)
            .into_iter()
            .filter(|label| !label.starts_with(REWIND_PREFIX))
            .map(|label| ref_name(session, &label))
            .collect();
        Subject {
            session: SessionId::new(session),
            folder: dir.to_path_buf(),
            recorded,
            rolled_back: HashSet::new(),
        }
    }

    fn idle(_: &SessionId) -> bool {
        false
    }

    fn probes(running: &dyn Fn(&SessionId) -> bool) -> Probes<'_> {
        Probes {
            now_secs: NOW,
            running,
        }
    }

    #[test]
    fn a_report_changes_nothing_and_lists_what_would_go() {
        // Arrange
        let repo = repo();
        for turn in 1..=5 {
            checkpoint(repo.path(), "1", &turn.to_string(), 1);
        }
        let before = refs_of(repo.path(), "1");

        // Act
        let result = prune(
            &[subject(repo.path(), "1")],
            &policy(),
            &probes(&idle),
            false,
        );

        // Assert
        assert_eq!(refs_of(repo.path(), "1"), before);
        let session = &result.sessions[0];
        assert_eq!(session.refs.len(), 2);
        assert!(session.refs.iter().all(|r| !r.removed));
        assert_eq!(session.kept, 3);
        assert!(session.bytes > 0);
    }

    #[test]
    fn turns_beyond_the_newest_few_go_and_the_base_and_the_newest_stay() {
        // Arrange
        let repo = repo();
        checkpoint(repo.path(), "1", "base", 1);
        for turn in [1, 2, 10, 11, 12] {
            checkpoint(repo.path(), "1", &turn.to_string(), 1);
        }

        // Act
        let result = prune(
            &[subject(repo.path(), "1")],
            &policy(),
            &probes(&idle),
            true,
        );

        // Assert
        assert_eq!(refs_of(repo.path(), "1"), ["10", "11", "12", "base"]);
        assert!(result.sessions[0].refs.iter().all(|r| r.removed));
    }

    #[test]
    fn old_turns_go_but_the_newest_stays_even_when_it_is_old() {
        // Arrange
        let repo = repo();
        checkpoint(repo.path(), "1", "1", 90);
        checkpoint(repo.path(), "1", "2", 60);
        checkpoint(repo.path(), "1", "3", 40);

        // Act
        prune(
            &[subject(repo.path(), "1")],
            &policy(),
            &probes(&idle),
            true,
        );

        // Assert
        assert_eq!(refs_of(repo.path(), "1"), ["3"]);
    }

    #[test]
    fn a_rewind_save_goes_sooner_than_a_turn() {
        // Arrange
        let repo = repo();
        checkpoint(repo.path(), "1", "1", 10);
        checkpoint(repo.path(), "1", "pre-rewind-111", 10);
        checkpoint(repo.path(), "1", "pre-rewind-222", 2);

        // Act
        prune(
            &[subject(repo.path(), "1")],
            &policy(),
            &probes(&idle),
            true,
        );

        // Assert
        assert_eq!(refs_of(repo.path(), "1"), ["1", "pre-rewind-222"]);
    }

    #[test]
    fn a_ref_with_an_unknown_name_stays() {
        // Arrange
        let repo = repo();
        checkpoint(repo.path(), "1", "mystery", 400);

        // Act
        let result = prune(
            &[subject(repo.path(), "1")],
            &policy(),
            &probes(&idle),
            true,
        );

        // Assert
        assert_eq!(refs_of(repo.path(), "1"), ["mystery"]);
        assert_eq!(result.sessions[0].refs.len(), 0);
    }

    #[test]
    fn other_refs_and_other_sessions_are_not_touched() {
        // Arrange
        let repo = repo();
        for turn in 1..=5 {
            checkpoint(repo.path(), "1", &turn.to_string(), 1);
            checkpoint(repo.path(), "2", &turn.to_string(), 1);
        }
        run(repo.path(), &["branch", "mine"], None);
        run(repo.path(), &["update-ref", "refs/tags/v1", "HEAD"], None);

        // Act
        prune(
            &[subject(repo.path(), "1")],
            &policy(),
            &probes(&idle),
            true,
        );

        // Assert
        assert_eq!(refs_of(repo.path(), "2").len(), 5);
        let heads = run(
            repo.path(),
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads",
                "refs/tags",
            ],
            None,
        );
        assert_eq!(heads.lines().count(), 3);
    }

    #[test]
    fn a_session_with_a_running_turn_is_left_alone() {
        // Arrange
        let repo = repo();
        for turn in 1..=5 {
            checkpoint(repo.path(), "1", &turn.to_string(), 1);
        }
        let running = |_: &SessionId| true;

        // Act
        let result = prune(
            &[subject(repo.path(), "1")],
            &policy(),
            &probes(&running),
            true,
        );

        // Assert
        assert_eq!(refs_of(repo.path(), "1").len(), 5);
        assert_eq!(
            result.sessions[0].note.as_deref(),
            Some("a turn is running")
        );
    }

    #[test]
    fn removing_the_refs_lets_git_collect_what_they_held() {
        // Arrange
        let repo = repo();
        for turn in 1..=5 {
            checkpoint(repo.path(), "1", &turn.to_string(), 1);
        }
        let first = run(
            repo.path(),
            &["rev-parse", &format!("{}1", ref_prefix("1"))],
            None,
        );

        // Act
        prune(
            &[subject(repo.path(), "1")],
            &policy(),
            &probes(&idle),
            true,
        );
        run(
            repo.path(),
            &["reflog", "expire", "--expire=now", "--all"],
            None,
        );
        run(repo.path(), &["gc", "-q", "--prune=now"], None);

        // Assert
        let exists = Command::new("git")
            .args(["cat-file", "-e", first.trim()])
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success();
        assert!(!exists);
    }

    #[test]
    fn a_session_without_refs_or_with_a_missing_folder_is_skipped() {
        // Arrange
        let repo = repo();
        let missing = TempDir::new().unwrap().path().join("gone");

        // Act
        let result = prune(
            &[
                subject(repo.path(), "9"),
                Subject {
                    session: SessionId::new("1"),
                    folder: missing,
                    recorded: HashSet::new(),
                    rolled_back: HashSet::new(),
                },
            ],
            &policy(),
            &probes(&idle),
            true,
        );

        // Assert
        assert_eq!(result.sessions.len(), 0);
    }

    #[test]
    fn refs_this_database_did_not_record_are_never_removed() {
        // Arrange
        let repo = repo();
        for turn in 1..=5 {
            checkpoint(repo.path(), "1", &turn.to_string(), 1);
        }
        let mut mine = subject(repo.path(), "1");
        mine.recorded.remove(&ref_name("1", "1"));
        mine.recorded.remove(&ref_name("1", "2"));
        checkpoint(repo.path(), "1", "pre-rewind-5", 100);

        // Act
        prune(&[mine], &policy(), &probes(&idle), true);

        // Assert
        assert_eq!(
            refs_of(repo.path(), "1"),
            ["1", "2", "3", "4", "5", "pre-rewind-5"]
        );
    }

    #[test]
    fn rewind_saves_are_kept_while_the_session_number_holds_unrecorded_refs() {
        // Arrange
        let repo = repo();
        checkpoint(repo.path(), "1", "1", 1);
        checkpoint(repo.path(), "1", "pre-rewind-1", 100);
        let mut mine = subject(repo.path(), "1");
        mine.recorded.clear();
        mine.recorded.insert(ref_name("1", "1"));
        checkpoint(repo.path(), "1", "7", 1);

        // Act
        prune(&[mine], &policy(), &probes(&idle), true);

        // Assert
        assert_eq!(refs_of(repo.path(), "1"), ["1", "7", "pre-rewind-1"]);
    }

    #[test]
    fn the_live_head_stays_when_later_turns_were_rolled_back() {
        // Arrange
        let repo = repo();
        checkpoint(repo.path(), "1", "5", 40);
        checkpoint(repo.path(), "1", "6", 39);
        checkpoint(repo.path(), "1", "7", 1);
        let mut mine = subject(repo.path(), "1");
        mine.rolled_back.insert(ref_name("1", "6"));
        mine.rolled_back.insert(ref_name("1", "7"));

        // Act
        prune(&[mine], &policy(), &probes(&idle), true);

        // Assert
        assert_eq!(refs_of(repo.path(), "1"), ["5", "7"]);
    }
}
