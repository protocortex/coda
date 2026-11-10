// SPDX-License-Identifier: Apache-2.0

//! Search over the messages and tool summaries earlier sessions stored.

use coda_protocol::readers::{RecallHit, RecallReader};
use coda_protocol::types::{BackendKind, Mode, SessionId, TurnId};
use coda_store::migrate::{self, STEPS};
use coda_store::{
    EventRecord, MAX_RECALL_RESULTS, MAX_RECALL_SNIPPET_BYTES, NewSession, SCHEMA_VERSION,
    SessionWorktree, Store,
};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

const REPO: &str = "/work/repo";
const BEFORE_RECALL: usize = 24;

fn open() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    store.configure_recall(true).unwrap();
    (dir, store)
}

fn session_in(store: &mut Store, backend: BackendKind, cwd: &str) -> SessionId {
    store
        .create_session(&NewSession {
            backend,
            model: None,
            cwd: cwd.to_owned(),
            mode: Mode::Default,
        })
        .unwrap()
}

fn say(store: &mut Store, session: &SessionId, kind: &str, body: &serde_json::Value) -> TurnId {
    let turn = store.begin_turn(session).unwrap();
    append(store, session, &turn, kind, body);
    turn
}

fn append(
    store: &mut Store,
    session: &SessionId,
    turn: &TurnId,
    kind: &str,
    body: &serde_json::Value,
) {
    let record = EventRecord {
        kind: kind.to_owned(),
        body: body.to_string(),
    };
    store.append_event(session, turn, &record).unwrap();
}

fn user(text: &str) -> serde_json::Value {
    json!({ "text": text })
}

fn found(store: &Store, asker: &SessionId, delegated: &[&str], query: &str) -> Vec<RecallHit> {
    let delegated: Vec<String> = delegated.iter().map(|repo| (*repo).to_owned()).collect();
    let reader = store.recall_reader(asker, &delegated);
    reader.search(query, MAX_RECALL_RESULTS).unwrap()
}

#[test]
fn a_phrase_from_an_earlier_session_is_found_with_its_session_and_turn() {
    // Arrange
    let (_dir, mut store) = open();
    let earlier = session_in(&mut store, BackendKind::Anthropic, REPO);
    let turn = say(
        &mut store,
        &earlier,
        "user_message",
        &user("the flaky reconnect test needs a fake clock"),
    );
    say(
        &mut store,
        &earlier,
        "assistant_message",
        &json!({ "text": "I moved the retry budget into config", "tool_calls": [] }),
    );
    let asker = session_in(&mut store, BackendKind::Anthropic, REPO);

    // Act
    let hits = found(&store, &asker, &[], "flaky reconnect");
    let reply = found(&store, &asker, &[], "retry budget");

    // Assert
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].session, earlier);
    assert_eq!(hits[0].turn, turn);
    assert_eq!(hits[0].seq, 1);
    assert_eq!(hits[0].kind, "user_message");
    assert!(hits[0].snippet.contains("flaky"), "{:?}", hits[0].snippet);
    assert_eq!(reply[0].kind, "assistant_message");
}

#[test]
fn results_are_capped_by_count_and_each_snippet_by_bytes() {
    // Arrange
    let (_dir, mut store) = open();
    let session = session_in(&mut store, BackendKind::Anthropic, REPO);
    let long = format!("needle {}", "word ".repeat(2000));
    for _ in 0..(MAX_RECALL_RESULTS + 5) {
        say(&mut store, &session, "user_message", &user(&long));
    }

    // Act
    let reader = store.recall_reader(&session, &[]);
    let few = reader.search("needle", 3).unwrap();
    let many = reader.search("needle", usize::MAX).unwrap();

    // Assert
    assert_eq!(few.len(), 3);
    assert_eq!(many.len(), MAX_RECALL_RESULTS);
    assert!(
        many.iter()
            .all(|hit| hit.snippet.len() <= MAX_RECALL_SNIPPET_BYTES)
    );
}

#[test]
fn a_deleted_session_disappears_from_the_results() {
    // Arrange
    let (_dir, mut store) = open();
    let gone = session_in(&mut store, BackendKind::Anthropic, REPO);
    say(
        &mut store,
        &gone,
        "user_message",
        &user("remember the walrus"),
    );
    let asker = session_in(&mut store, BackendKind::Anthropic, REPO);
    assert_eq!(found(&store, &asker, &[], "walrus").len(), 1);

    // Act
    store.delete_session(&gone).unwrap();

    // Assert
    assert_eq!(
        found(&store, &asker, &[], "walrus"),
        Vec::<RecallHit>::new()
    );
}

#[test]
fn nothing_is_indexed_while_recall_is_off() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let session = session_in(&mut store, BackendKind::Anthropic, REPO);

    // Act
    say(
        &mut store,
        &session,
        "user_message",
        &user("secret walrus plan"),
    );

    // Assert
    assert_eq!(
        found(&store, &session, &[], "walrus"),
        Vec::<RecallHit>::new()
    );
    let indexed: i64 = Connection::open(dir.path().join("data/coda.db"))
        .unwrap()
        .query_row("SELECT count(*) FROM recall_entries", [], |r| r.get(0))
        .unwrap();
    assert_eq!(indexed, 0);
}

#[test]
fn turning_recall_off_clears_what_was_indexed() {
    // Arrange
    let (_dir, mut store) = open();
    let session = session_in(&mut store, BackendKind::Anthropic, REPO);
    say(
        &mut store,
        &session,
        "user_message",
        &user("the walrus again"),
    );
    assert_eq!(found(&store, &session, &[], "walrus").len(), 1);

    // Act
    store.configure_recall(false).unwrap();

    // Assert
    assert_eq!(
        found(&store, &session, &[], "walrus"),
        Vec::<RecallHit>::new()
    );
}

#[test]
fn turning_recall_on_indexes_what_was_stored_before() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let session = session_in(&mut store, BackendKind::Anthropic, REPO);
    say(
        &mut store,
        &session,
        "user_message",
        &user("an older walrus note"),
    );

    // Act
    store.configure_recall(true).unwrap();
    store.configure_recall(true).unwrap();

    // Assert
    assert_eq!(found(&store, &session, &[], "walrus").len(), 1);
}

#[test]
fn a_tool_summary_is_found_and_leaves_with_its_deleted_original() {
    // Arrange
    let (_dir, mut store) = open();
    let session = session_in(&mut store, BackendKind::Anthropic, REPO);
    let original = store
        .store_original(&session, "error: linker `cc` not found\nmore lines")
        .unwrap();
    let turn = store.begin_turn(&session).unwrap();
    append(
        &mut store,
        &session,
        &turn,
        "tool_finished",
        &json!({"tool": "bash", "call_id": "c1", "original": original.as_str(), "is_error": true}),
    );
    let before = found(&store, &session, &[], "linker");

    // Act
    store.delete_original(&session, &original).unwrap();

    // Assert
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].kind, "tool_finished");
    assert_eq!(
        found(&store, &session, &[], "linker"),
        Vec::<RecallHit>::new()
    );
}

#[test]
fn a_delegated_session_is_searched_only_for_a_repository_the_user_listed() {
    // Arrange
    let (_dir, mut store) = open();
    let delegated = session_in(&mut store, BackendKind::ClaudeCode, REPO);
    say(
        &mut store,
        &delegated,
        "user_message",
        &user("ask about the walrus"),
    );
    let asker = session_in(&mut store, BackendKind::Anthropic, REPO);

    // Act
    let unlisted = found(&store, &asker, &[], "walrus");
    let other_repo = found(&store, &asker, &["/work/elsewhere"], "walrus");
    let listed = found(&store, &asker, &[REPO], "walrus");

    // Assert
    assert_eq!(unlisted, Vec::<RecallHit>::new());
    assert_eq!(other_repo, Vec::<RecallHit>::new());
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session, delegated);
}

#[test]
fn a_session_in_a_worktree_belongs_to_the_repository_it_was_made_from() {
    // Arrange
    let (_dir, mut store) = open();
    let earlier = session_in(&mut store, BackendKind::Anthropic, REPO);
    say(
        &mut store,
        &earlier,
        "user_message",
        &user("walrus in the main checkout"),
    );
    let asker = session_in(&mut store, BackendKind::Anthropic, "/data/worktrees/one");
    store
        .set_session_worktree(
            &asker,
            &SessionWorktree {
                path: "/data/worktrees/one".to_owned(),
                branch: "coda/one".to_owned(),
                repo: REPO.to_owned(),
            },
        )
        .unwrap();

    // Act
    let hits = found(&store, &asker, &[], "walrus");

    // Assert
    assert_eq!(hits.len(), 1);
}

#[test]
fn text_from_a_rolled_back_turn_is_not_found() {
    // Arrange
    let (_dir, mut store) = open();
    let session = session_in(&mut store, BackendKind::Anthropic, REPO);
    say(&mut store, &session, "user_message", &user("walrus kept"));
    let undone = say(&mut store, &session, "user_message", &user("walrus undone"));
    let first = store.load_events(&session, 0, 1).unwrap()[0].turn.clone();

    // Act
    store.roll_back_turns_after(&session, Some(&first)).unwrap();

    // Assert
    let hits = found(&store, &session, &[], "walrus");
    assert_eq!(hits.len(), 1);
    assert_ne!(hits[0].turn, undone);
}

#[test]
fn query_syntax_in_the_words_is_read_as_plain_text() {
    // Arrange
    let (_dir, mut store) = open();
    let session = session_in(&mut store, BackendKind::Anthropic, REPO);
    say(
        &mut store,
        &session,
        "user_message",
        &user("walrus and seal"),
    );
    let reader = store.recall_reader(&session, &[]);

    for query in [
        "\"",
        "(",
        "walrus AND",
        "NOT",
        "text:walrus",
        "*",
        "NEAR(a b)",
    ] {
        // Act
        let hits = reader.search(query, MAX_RECALL_RESULTS);

        // Assert
        assert!(hits.is_ok(), "{query}: {hits:?}");
    }
}

#[test]
fn a_v24_database_gains_the_index_and_keeps_its_sessions() {
    // Arrange
    let mut old = Connection::open_in_memory().unwrap();
    old.pragma_update(None, "foreign_keys", true).unwrap();
    migrate::migrate_with(&mut old, &STEPS[..BEFORE_RECALL], env!("CARGO_PKG_VERSION")).unwrap();
    old.execute_batch("INSERT INTO sessions (id, created_at) VALUES (1, 0);")
        .unwrap();
    let mut fresh = Connection::open_in_memory().unwrap();
    migrate::migrate(&mut fresh).unwrap();

    // Act
    migrate::migrate(&mut old).unwrap();

    // Assert
    let version: u32 = old
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let sessions: i64 = old
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sessions, 1);
    assert_eq!(listing(&old), listing(&fresh));
}

fn listing(conn: &Connection) -> Vec<(String, String, Option<String>)> {
    let mut stmt = conn
        .prepare(
            "SELECT type, name, sql FROM sqlite_master \
             WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}
