// SPDX-License-Identifier: AGPL-3.0-only

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use coda_protocol::types::{BackendKind, Mode, PermissionDecision, RequestId, SessionId, TurnId};
use coda_store::{
    Clock, EventRecord, GrantKey, NewDecision, NewPending, NewSession, SessionFilter, Store,
    StoreError, UsageRecord, UsageSummary,
};
use rusqlite::Connection;
use tempfile::TempDir;

const T0: i64 = 1_700_000_000_000;
const V1_SNAPSHOT: &str = include_str!("snapshots/v1.sql");

#[derive(Debug)]
struct FixedClock(AtomicI64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn open_at(data_dir: &Path) -> Store {
    let clock = Arc::new(FixedClock(AtomicI64::new(T0)));
    Store::open_with_clock(data_dir, clock).unwrap()
}

fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = open_at(&dir.path().join("data"));
    (dir, store)
}

fn event(kind: &str, body: &str) -> EventRecord {
    EventRecord {
        kind: kind.to_owned(),
        body: body.to_owned(),
    }
}

fn new_session(cwd: &str) -> NewSession {
    NewSession {
        backend: BackendKind::Faux,
        model: None,
        cwd: cwd.to_owned(),
        mode: Mode::Default,
    }
}

#[test]
fn created_session_loads_back_with_the_same_fields() {
    // Arrange
    let (_dir, mut store) = open_store();
    let new = NewSession {
        backend: BackendKind::ClaudeCode,
        model: Some("opus".to_owned()),
        cwd: "/work/repo".to_owned(),
        mode: Mode::Plan,
    };

    // Act
    let id = store.create_session(&new).unwrap();
    let loaded = store.load_session(&id).unwrap();

    // Assert
    assert_eq!(loaded.id, id);
    assert_eq!(loaded.backend, BackendKind::ClaudeCode);
    assert_eq!(loaded.model.as_deref(), Some("opus"));
    assert_eq!(loaded.cwd, "/work/repo");
    assert_eq!(loaded.mode, Mode::Plan);
    assert_eq!(loaded.created_at, T0);
}

#[test]
fn listing_with_no_sessions_is_an_empty_vec() {
    // Arrange
    let (_dir, store) = open_store();

    // Act
    let listed = store.list_sessions(&SessionFilter::default()).unwrap();

    // Assert
    assert_eq!(listed, Vec::new());
}

#[test]
fn listing_returns_sessions_and_filters_by_cwd() {
    // Arrange
    let (_dir, mut store) = open_store();
    let a = store.create_session(&new_session("/a")).unwrap();
    let b = store.create_session(&new_session("/b")).unwrap();

    // Act
    let all = store.list_sessions(&SessionFilter::default()).unwrap();
    let only_b = store
        .list_sessions(&SessionFilter {
            cwd: Some("/b".to_owned()),
        })
        .unwrap();

    // Assert
    let all_ids: Vec<_> = all.into_iter().map(|s| s.id).collect();
    assert_eq!(all_ids, vec![a, b.clone()]);
    assert_eq!(only_b.len(), 1);
    assert_eq!(only_b[0].id, b);
}

#[test]
fn loading_a_nonexistent_session_is_session_not_found() {
    // Arrange
    let (_dir, store) = open_store();
    let missing = SessionId::new("404");

    // Act
    let err = store.load_session(&missing).unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::SessionNotFound { session } if session == missing));
}

#[test]
fn set_session_model_updates_the_stored_model() {
    // Arrange
    let (_dir, mut store) = open_store();
    let id = store.create_session(&new_session("/a")).unwrap();

    // Act
    store.set_session_model(&id, "sonnet").unwrap();

    // Assert
    assert_eq!(
        store.load_session(&id).unwrap().model.as_deref(),
        Some("sonnet")
    );
}

#[test]
fn setting_the_model_of_a_nonexistent_session_is_session_not_found() {
    // Arrange
    let (_dir, mut store) = open_store();

    // Act
    let err = store
        .set_session_model(&SessionId::new("7"), "sonnet")
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::SessionNotFound { .. }));
}

#[test]
fn appends_return_increasing_sequence_numbers_and_load_in_order() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();

    // Act
    let seqs: Vec<u64> = ["a", "b", "c"]
        .iter()
        .map(|body| {
            store
                .append_event(&session, &turn, &event("text", body))
                .unwrap()
        })
        .collect();
    let loaded = store.load_events(&session, 0, 10).unwrap();

    // Assert
    assert_eq!(seqs, vec![1, 2, 3]);
    let bodies: Vec<_> = loaded.iter().map(|e| (e.seq, e.body.as_str())).collect();
    assert_eq!(bodies, vec![(1, "a"), (2, "b"), (3, "c")]);
    assert_eq!(loaded[0].kind, "text");
    assert_eq!(loaded[0].turn, turn);
    assert_eq!(loaded[0].at, T0);
}

#[test]
fn sequence_numbers_continue_across_turns_of_a_session() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let first = store.begin_turn(&session).unwrap();
    let second = store.begin_turn(&session).unwrap();
    store
        .append_event(&session, &first, &event("k", "a"))
        .unwrap();

    // Act
    let seq = store
        .append_event(&session, &second, &event("k", "b"))
        .unwrap();

    // Assert
    assert_eq!(seq, 2);
}

#[test]
fn load_events_honours_after_seq_and_limit() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();
    for body in ["a", "b", "c", "d"] {
        store
            .append_event(&session, &turn, &event("k", body))
            .unwrap();
    }

    // Act
    let page = store.load_events(&session, 1, 2).unwrap();

    // Assert
    let seqs: Vec<_> = page.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, vec![2, 3]);
}

#[test]
fn appending_to_a_nonexistent_session_is_session_not_found() {
    // Arrange
    let (_dir, mut store) = open_store();
    let missing = SessionId::new("9");

    // Act
    let err = store
        .append_event(&missing, &TurnId::new("1"), &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::SessionNotFound { .. }));
}

#[test]
fn appending_to_a_turn_of_another_session_is_turn_not_found() {
    // Arrange
    let (_dir, mut store) = open_store();
    let one = store.create_session(&new_session("/a")).unwrap();
    let two = store.create_session(&new_session("/b")).unwrap();
    let turn_of_two = store.begin_turn(&two).unwrap();

    // Act
    let err = store
        .append_event(&one, &turn_of_two, &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::TurnNotFound { .. }));
}

#[test]
fn appending_an_existing_sequence_number_is_a_duplicate_append() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();
    store
        .append_event_at(&session, &turn, 1, &event("k", "a"))
        .unwrap();

    // Act
    let err = store
        .append_event_at(&session, &turn, 1, &event("k", "again"))
        .unwrap_err();

    // Assert
    assert!(matches!(
        err,
        StoreError::DuplicateAppend { session: s, seq: 1 } if s == session
    ));
    assert_eq!(store.load_events(&session, 0, 10).unwrap().len(), 1);
}

#[test]
fn a_second_writer_on_the_same_session_is_rejected() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut first = open_at(&data_dir);
    let mut second = open_at(&data_dir);
    let session = first.create_session(&new_session("/a")).unwrap();
    let turn = first.begin_turn(&session).unwrap();
    first
        .append_event(&session, &turn, &event("k", "a"))
        .unwrap();

    // Act
    let err = second
        .append_event(&session, &turn, &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::WriterBusy { session: s } if s == session));
}

#[test]
fn dropping_the_writer_lets_another_handle_append() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut first = open_at(&data_dir);
    let mut second = open_at(&data_dir);
    let session = first.create_session(&new_session("/a")).unwrap();
    let turn = first.begin_turn(&session).unwrap();
    first
        .append_event(&session, &turn, &event("k", "a"))
        .unwrap();
    drop(first);

    // Act
    let seq = second
        .append_event(&session, &turn, &event("k", "b"))
        .unwrap();

    // Assert
    assert_eq!(seq, 2);
}

#[test]
fn a_database_failure_while_checking_the_session_is_a_sqlite_error() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut store = open_at(&data_dir);
    let session = store.create_session(&new_session("/a")).unwrap();
    Connection::open(data_dir.join("coda.db"))
        .unwrap()
        .execute_batch("DROP TABLE sessions")
        .unwrap();

    // Act
    let err = store.load_events(&session, 0, 10).unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::Sqlite(_)), "got {err:?}");
}

#[test]
fn a_database_failure_while_checking_the_turn_is_a_sqlite_error() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut store = open_at(&data_dir);
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();
    Connection::open(data_dir.join("coda.db"))
        .unwrap()
        .execute_batch("DROP TABLE turns")
        .unwrap();

    // Act
    let err = store
        .append_event(&session, &turn, &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::Sqlite(_)), "got {err:?}");
}

#[test]
fn a_non_unique_constraint_failure_is_not_a_duplicate_append() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut store = open_at(&data_dir);
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();
    Connection::open(data_dir.join("coda.db"))
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER reject_events BEFORE INSERT ON events \
             BEGIN SELECT RAISE(ABORT, 'rejected'); END",
        )
        .unwrap();

    // Act
    let err = store
        .append_event_at(&session, &turn, 1, &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::Sqlite(_)), "got {err:?}");
}

#[test]
fn appending_at_sequence_zero_is_out_of_range() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let turn = store.begin_turn(&session).unwrap();

    // Act
    let err = store
        .append_event_at(&session, &turn, 0, &event("k", "b"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::ValueOutOfRange { value: 0 }));
    let stored = store.load_events(&session, 0, 10).unwrap();
    assert!(matches!(stored.as_slice(), []));
}

#[test]
fn a_failed_append_does_not_hold_the_writer_lease() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut first = open_at(&data_dir);
    let mut second = open_at(&data_dir);
    let session = first.create_session(&new_session("/a")).unwrap();
    let turn = first.begin_turn(&session).unwrap();
    let failed = first.append_event(&session, &TurnId::new("999"), &event("k", "a"));

    // Act
    let seq = second
        .append_event(&session, &turn, &event("k", "b"))
        .unwrap();

    // Assert
    assert!(matches!(failed, Err(StoreError::TurnNotFound { .. })));
    assert_eq!(seq, 1);
}

#[test]
fn a_v1_session_row_upgrades_with_the_v2_defaults() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    fs::create_dir(&data_dir).unwrap();
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let db_path = data_dir.join("coda.db");
    let old = Connection::open(&db_path).unwrap();
    old.execute_batch(V1_SNAPSHOT).unwrap();
    old.execute("INSERT INTO sessions (id, created_at) VALUES (7, ?1)", [T0])
        .unwrap();
    old.pragma_update(None, "user_version", 1).unwrap();
    drop(old);
    fs::set_permissions(&db_path, fs::Permissions::from_mode(0o600)).unwrap();

    // Act
    let store = open_at(&data_dir);
    let loaded = store.load_session(&SessionId::new("7")).unwrap();

    // Assert
    assert_eq!(loaded.backend, BackendKind::Faux);
    assert_eq!(loaded.mode, Mode::Default);
    assert_eq!(loaded.cwd, "");
    assert_eq!(loaded.model, None);
    assert_eq!(loaded.created_at, T0);
}

#[test]
fn a_panic_while_holding_a_store_releases_the_writer_lease() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let data_dir = dir.path().join("data");
    let mut first = open_at(&data_dir);
    let mut second = open_at(&data_dir);
    let session = first.create_session(&new_session("/a")).unwrap();
    let turn = first.begin_turn(&session).unwrap();

    // Act
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        first
            .append_event(&session, &turn, &event("k", "a"))
            .unwrap();
        panic!("simulated failure while holding the store");
    }));
    drop(first);
    let seq = second
        .append_event(&session, &turn, &event("k", "b"))
        .unwrap();

    // Assert
    assert!(outcome.is_err());
    assert_eq!(seq, 2);
}

fn session_with_turn(store: &mut Store, cwd: &str) -> (SessionId, TurnId) {
    let session = store.create_session(&new_session(cwd)).unwrap();
    let turn = store.begin_turn(&session).unwrap();
    (session, turn)
}

#[test]
fn usage_is_recorded_and_summed_per_session_and_overall() {
    // Arrange
    let (_dir, mut store) = open_store();
    let (one, turn_one) = session_with_turn(&mut store, "/a");
    let (two, turn_two) = session_with_turn(&mut store, "/b");
    for (session, turn, input, output) in [
        (&one, &turn_one, 100, 10),
        (&one, &turn_one, 50, 5),
        (&two, &turn_two, 7, 3),
    ] {
        store
            .record_usage(&UsageRecord {
                session: session.clone(),
                turn: turn.clone(),
                input_tokens: input,
                output_tokens: output,
            })
            .unwrap();
    }

    // Act
    let for_one = store.usage_summary(Some(&one)).unwrap();
    let overall = store.usage_summary(None).unwrap();

    // Assert
    assert_eq!(
        for_one,
        UsageSummary {
            input_tokens: 150,
            output_tokens: 15
        }
    );
    assert_eq!(
        overall,
        UsageSummary {
            input_tokens: 157,
            output_tokens: 18
        }
    );
}

#[test]
fn usage_summary_with_no_records_is_zero() {
    // Arrange
    let (_dir, store) = open_store();

    // Act
    let summary = store.usage_summary(None).unwrap();

    // Assert
    assert_eq!(summary, UsageSummary::default());
}

#[test]
fn recording_usage_for_a_nonexistent_turn_is_turn_not_found() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();

    // Act
    let err = store
        .record_usage(&UsageRecord {
            session,
            turn: TurnId::new("5"),
            input_tokens: 1,
            output_tokens: 1,
        })
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::TurnNotFound { .. }));
}

fn new_pending(session: &SessionId, command: &str) -> NewPending {
    NewPending {
        session: session.clone(),
        tool: "bash".to_owned(),
        repo_path: "/repo".to_owned(),
        command: command.to_owned(),
    }
}

#[test]
fn pending_permissions_are_added_listed_and_resolved() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let id = store
        .add_pending(&new_pending(&session, "cargo test"))
        .unwrap();

    // Act
    let before = store.list_pending(&session).unwrap();
    store
        .resolve_pending(&id, PermissionDecision::AllowOnce)
        .unwrap();
    let after = store.list_pending(&session).unwrap();

    // Assert
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].id, id);
    assert_eq!(before[0].command, "cargo test");
    assert_eq!(before[0].tool, "bash");
    assert_eq!(before[0].repo_path, "/repo");
    assert_eq!(before[0].requested_at, T0);
    assert_eq!(after, Vec::new());
}

#[test]
fn resolving_a_pending_permission_twice_is_already_resolved() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let id = store.add_pending(&new_pending(&session, "ls")).unwrap();
    store
        .resolve_pending(&id, PermissionDecision::Deny)
        .unwrap();

    // Act
    let err = store
        .resolve_pending(&id, PermissionDecision::AllowOnce)
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::AlreadyResolved { request } if request == id));
    let decisions = store.list_decisions(&session).unwrap();
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].allowed);
}

#[test]
fn non_canonical_request_ids_are_not_found() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let id = store.add_pending(&new_pending(&session, "ls")).unwrap();
    assert_eq!(id.as_str(), "1");

    for raw_id in ["01", "+1", "1 ", "-0"] {
        // Act
        let err = store
            .resolve_pending(&RequestId::new(raw_id), PermissionDecision::Deny)
            .unwrap_err();

        // Assert
        assert!(
            matches!(err, StoreError::PendingNotFound { .. }),
            "{raw_id:?}"
        );
    }
    assert_eq!(store.list_pending(&session).unwrap().len(), 1);
}

#[test]
fn resolving_an_unknown_pending_permission_is_pending_not_found() {
    // Arrange
    let (_dir, mut store) = open_store();

    // Act
    let err = store
        .resolve_pending(&RequestId::new("3"), PermissionDecision::Deny)
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::PendingNotFound { .. }));
}

#[test]
fn adding_a_pending_permission_to_a_nonexistent_session_is_session_not_found() {
    // Arrange
    let (_dir, mut store) = open_store();

    // Act
    let err = store
        .add_pending(&new_pending(&SessionId::new("8"), "ls"))
        .unwrap_err();

    // Assert
    assert!(matches!(err, StoreError::SessionNotFound { .. }));
}

#[test]
fn decisions_are_recorded_and_listed_for_a_session() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let decision = |command: &str, decision| NewDecision {
        session: session.clone(),
        repo_path: "/repo".to_owned(),
        command: command.to_owned(),
        decision,
    };

    // Act
    store
        .record_decision(&decision("ls", PermissionDecision::AllowAlways))
        .unwrap();
    store
        .record_decision(&decision("rm -rf x", PermissionDecision::Deny))
        .unwrap();
    let decisions = store.list_decisions(&session).unwrap();

    // Assert
    let got: Vec<_> = decisions
        .iter()
        .map(|d| (d.command.as_str(), d.allowed))
        .collect();
    assert_eq!(got, vec![("ls", true), ("rm -rf x", false)]);
    assert_eq!(decisions[0].decided_at, T0);
}

#[test]
fn resolving_a_pending_permission_records_a_decision() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = store.create_session(&new_session("/a")).unwrap();
    let id = store.add_pending(&new_pending(&session, "make")).unwrap();

    // Act
    store
        .resolve_pending(&id, PermissionDecision::Deny)
        .unwrap();

    // Assert
    let decisions = store.list_decisions(&session).unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].command, "make");
    assert!(!decisions[0].allowed);
}

fn key<'a>(repo_path: &'a str, tool: &'a str, cwd: &'a str, pattern: &'a str) -> GrantKey<'a> {
    GrantKey {
        repo_path,
        tool,
        cwd,
        pattern,
    }
}

fn grant_count(dir: &TempDir) -> i64 {
    Connection::open(dir.path().join("data/coda.db"))
        .unwrap()
        .query_row("SELECT count(*) FROM permission_grants", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_saved_grant_is_found_for_its_full_key() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .save_grant(&key("/repo", "Bash", "/repo/x", "cargo test"))
        .unwrap();

    // Act
    let found = store
        .lookup_grant(&key("/repo", "Bash", "/repo/x", "cargo test"))
        .unwrap();

    // Assert
    let grant = found.unwrap();
    assert_eq!(grant.repo_path, "/repo");
    assert_eq!(grant.tool, "Bash");
    assert_eq!(grant.cwd, "/repo/x");
    assert_eq!(grant.command_pattern, "cargo test");
    assert_eq!(grant.granted_at, T0);
}

#[test]
fn a_grant_lookup_with_no_match_is_none() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .save_grant(&key("/repo", "Bash", "/repo", "cargo test"))
        .unwrap();

    // Act
    let found = store
        .lookup_grant(&key("/repo", "Bash", "/repo", "cargo publish"))
        .unwrap();

    // Assert
    assert_eq!(found, None);
}

#[test]
fn grants_are_scoped_to_their_repo() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .save_grant(&key("/repo-a", "Bash", "/repo-a", "cargo test"))
        .unwrap();

    // Act
    let other_repo = store
        .lookup_grant(&key("/repo-b", "Bash", "/repo-a", "cargo test"))
        .unwrap();

    // Assert
    assert_eq!(other_repo, None);
}

#[test]
fn a_grant_in_one_cwd_does_not_match_another_cwd() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .save_grant(&key("/repo", "Bash", "/repo/x", "cargo test"))
        .unwrap();

    // Act
    let other_cwd = store
        .lookup_grant(&key("/repo", "Bash", "/repo/y", "cargo test"))
        .unwrap();

    // Assert
    assert_eq!(other_cwd, None);
}

#[test]
fn a_grant_for_one_tool_does_not_match_another_tool() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .save_grant(&key("/repo", "Bash", "/repo", "cargo test"))
        .unwrap();

    // Act
    let other_tool = store
        .lookup_grant(&key("/repo", "Edit", "/repo", "cargo test"))
        .unwrap();

    // Assert
    assert_eq!(other_tool, None);
}

#[test]
fn repo_paths_match_exactly_never_by_prefix() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .save_grant(&key("/repo", "Bash", "/repo", "ls"))
        .unwrap();

    for other in ["/repo/sub", "/repo2"] {
        // Act
        let found = store
            .lookup_grant(&key(other, "Bash", "/repo", "ls"))
            .unwrap();

        // Assert
        assert_eq!(found, None, "{other}");
    }
}

#[test]
fn patterns_match_exactly_never_by_prefix_or_whitespace() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .save_grant(&key("/repo", "Bash", "/repo", "cargo test"))
        .unwrap();

    for other in [
        "cargo test; rm -rf x",
        "cargo  test",
        "cargo test ",
        "cargo",
    ] {
        // Act
        let found = store
            .lookup_grant(&key("/repo", "Bash", "/repo", other))
            .unwrap();

        // Assert
        assert_eq!(found, None, "{other:?}");
    }
}

#[test]
fn saving_the_same_grant_twice_keeps_one_row_and_the_first_grant_time() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(FixedClock(AtomicI64::new(T0)));
    let mut store = Store::open_with_clock(&dir.path().join("data"), clock.clone()).unwrap();
    store
        .save_grant(&key("/repo", "Bash", "/repo", "ls"))
        .unwrap();
    clock.0.store(T0 + 5_000, Ordering::SeqCst);

    // Act
    store
        .save_grant(&key("/repo", "Bash", "/repo", "ls"))
        .unwrap();

    // Assert
    assert_eq!(grant_count(&dir), 1);
    let grant = store
        .lookup_grant(&key("/repo", "Bash", "/repo", "ls"))
        .unwrap()
        .unwrap();
    assert_eq!(grant.granted_at, T0);
}

#[test]
fn summaries_report_the_last_activity_and_filter_by_cwd() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let clock = Arc::new(FixedClock(AtomicI64::new(T0)));
    let mut store = Store::open_with_clock(&dir.path().join("data"), clock.clone()).unwrap();
    let quiet = store.create_session(&new_session("/a")).unwrap();
    let busy = store.create_session(&new_session("/a")).unwrap();
    let other = store.create_session(&new_session("/b")).unwrap();
    let turn = store.begin_turn(&busy).unwrap();
    clock.0.store(T0 + 7_000, Ordering::SeqCst);
    store
        .append_event(&busy, &turn, &event("user_message", "{}"))
        .unwrap();

    // Act
    let in_a = store
        .list_session_summaries(
            &SessionFilter {
                cwd: Some("/a".to_owned()),
            },
            10,
        )
        .unwrap();
    let all = store
        .list_session_summaries(&SessionFilter::default(), 10)
        .unwrap();

    // Assert
    let seen: Vec<_> = in_a.iter().map(|s| (&s.record.id, s.updated_at)).collect();
    assert_eq!(seen, vec![(&busy, T0 + 7_000), (&quiet, T0)]);
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].record.id, other);
}

#[test]
fn summaries_keep_only_the_newest_sessions_within_the_limit() {
    // Arrange
    let (_dir, mut store) = open_store();
    let ids: Vec<_> = (0..4)
        .map(|_| store.create_session(&new_session("/a")).unwrap())
        .collect();

    // Act
    let capped = store
        .list_session_summaries(&SessionFilter::default(), 2)
        .unwrap();

    // Assert
    let seen: Vec<_> = capped.iter().map(|s| &s.record.id).collect();
    assert_eq!(seen, vec![&ids[3], &ids[2]]);
}
