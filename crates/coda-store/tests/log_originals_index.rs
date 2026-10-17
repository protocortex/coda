// SPDX-License-Identifier: AGPL-3.0-only

use coda_protocol::readers::{IndexReader, OriginalsReader, ReaderError, SymbolLocation};
use coda_protocol::types::{BackendKind, Mode, OriginalId, SessionId};
use coda_store::{
    LogEvent, LogEventKind, LogScope, MAX_LOG_ID, MAX_ORIGINAL_BYTES, MAX_SESSION_ORIGINAL_BYTES,
    NewSession, Store, StoreError, Symbol,
};
use rusqlite::Connection;
use tempfile::TempDir;

/// Every TEXT column allowed in the content-free tables, each constrained by `CHECK ... IN`.
const ALLOWED_TEXT_COLUMNS: &[(&str, &str)] = &[("event_log", "kind"), ("event_log", "scope")];
type LogRow = (
    String,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<i64>,
);
const CANARY: &str = "canary secret text";

/// True for any column that is not INTEGER and not an allowlisted text column.
fn is_unsafe_column(table: &str, name: &str, ty: &str) -> bool {
    ty.trim().to_uppercase() != "INTEGER"
        && !ALLOWED_TEXT_COLUMNS
            .iter()
            .any(|(t, c)| t.eq_ignore_ascii_case(table) && c.eq_ignore_ascii_case(name))
}

#[test]
fn column_predicate_flags_every_non_integer_type_outside_the_allowlist() {
    // Arrange
    let cases = [
        ("event_log", "note", "TEXT", true),
        ("event_log", "note", "text", true),
        ("event_log", "note", "BLOB", true),
        ("event_log", "note", "ANY", true),
        ("event_log", "note", "", true),
        ("event_log", "at", "integer", false),
        ("event_log", "kind", "text", false),
    ];

    // Act
    let results: Vec<bool> = cases
        .iter()
        .map(|(t, c, ty, _)| is_unsafe_column(t, c, ty))
        .collect();

    // Assert
    let expected: Vec<bool> = cases.iter().map(|c| c.3).collect();
    assert_eq!(results, expected);
}

fn open_store() -> (TempDir, Store) {
    let dir = TempDir::new().unwrap();
    let store = Store::open(&dir.path().join("data")).unwrap();
    (dir, store)
}

fn raw(dir: &TempDir) -> Connection {
    let conn = Connection::open(dir.path().join("data/coda.db")).unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    conn
}

fn new_session(store: &mut Store) -> SessionId {
    store
        .create_session(&NewSession {
            backend: BackendKind::Faux,
            model: None,
            cwd: "/work".to_owned(),
            mode: Mode::Default,
        })
        .unwrap()
}

fn event(kind: LogEventKind) -> LogEvent {
    LogEvent {
        kind,
        session: None,
        turn: None,
        scope: None,
        producer_id: None,
    }
}

fn symbol(name: &str, kind: &str, line: u32) -> Symbol {
    Symbol {
        name: name.to_owned(),
        kind: kind.to_owned(),
        line,
    }
}

#[test]
fn raw_sql_rejects_a_free_text_kind_in_event_log() {
    // Arrange
    let (dir, _store) = open_store();
    let conn = raw(&dir);

    // Act
    let valid = conn.execute("INSERT INTO event_log (kind, at) VALUES ('error', 1)", []);
    let result = conn.execute("INSERT INTO event_log (kind, at) VALUES (?1, 1)", [CANARY]);

    // Assert
    assert!(valid.is_ok());
    assert!(result.is_err());
}

#[test]
fn raw_sql_rejects_free_text_in_event_log_and_usage_columns() {
    // Arrange
    let (dir, mut store) = open_store();
    new_session(&mut store);
    let conn = raw(&dir);
    conn.execute(
        "INSERT INTO turns (session_id, started_at) VALUES (1, 1)",
        [],
    )
    .unwrap();

    // Act
    let scope = conn.execute(
        "INSERT INTO event_log (kind, at, scope) VALUES ('error', 1, ?1)",
        [CANARY],
    );
    let producer = conn.execute(
        "INSERT INTO event_log (kind, at, producer_id) VALUES ('error', 1, ?1)",
        [CANARY],
    );
    let tokens = conn.execute(
        "INSERT INTO usage (turn_id, input_tokens, output_tokens, at) VALUES (1, ?1, 0, 1)",
        [CANARY],
    );

    // Assert
    assert!(scope.is_err());
    assert!(producer.is_err());
    assert!(tokens.is_err());
}

#[test]
fn text_columns_in_content_free_tables_are_allowlisted_and_constrained() {
    // Arrange
    let (dir, _store) = open_store();
    let conn = raw(&dir);
    let mut found = Vec::new();

    // Act
    for table in ["event_log", "usage"] {
        let mut stmt = conn
            .prepare("SELECT name, type FROM pragma_table_info(?1)")
            .unwrap();
        let columns = stmt
            .query_map([table], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .unwrap()
            .map(Result::unwrap);
        for (name, ty) in columns {
            if is_unsafe_column(table, &name, &ty) {
                found.push((table.to_owned(), name));
            }
        }
    }

    // Assert
    assert_eq!(found, Vec::<(String, String)>::new());
    for (table, column) in ALLOWED_TEXT_COLUMNS {
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = ?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            sql.contains(&format!("CHECK ({column} IN")),
            "{table}.{column} lacks an IN constraint"
        );
    }
}

#[test]
fn scope_and_producer_id_are_nullable_and_round_trip() {
    // Arrange
    let (dir, mut store) = open_store();
    let scoped = LogEvent {
        kind: LogEventKind::ToolCalled,
        session: Some(4),
        turn: Some(5),
        scope: Some(LogScope::Repo),
        producer_id: Some(9),
    };

    // Act
    store.record_log_event(&scoped).unwrap();
    store.record_log_event(&event(LogEventKind::Error)).unwrap();

    // Assert
    let conn = raw(&dir);
    let rows: Vec<LogRow> = conn
        .prepare("SELECT kind, session_id, turn_id, scope, producer_id FROM event_log ORDER BY id")
        .unwrap()
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        rows,
        vec![
            (
                "tool_called".to_owned(),
                Some(4),
                Some(5),
                Some("repo".to_owned()),
                Some(9)
            ),
            ("error".to_owned(), None, None, None, None),
        ]
    );
}

#[test]
fn typed_api_rejects_oversize_ids() {
    // Arrange
    let (_dir, mut store) = open_store();
    let too_big = MAX_LOG_ID + 1;

    // Act
    let session = store.record_log_event(&LogEvent {
        session: Some(too_big),
        ..event(LogEventKind::Error)
    });
    let producer = store.record_log_event(&LogEvent {
        producer_id: Some(too_big),
        ..event(LogEventKind::Error)
    });
    let at_max = store.record_log_event(&LogEvent {
        turn: Some(MAX_LOG_ID),
        ..event(LogEventKind::Error)
    });

    // Assert
    assert!(matches!(
        session,
        Err(StoreError::LogValueTooLarge {
            field: "session",
            ..
        })
    ));
    assert!(matches!(
        producer,
        Err(StoreError::LogValueTooLarge {
            field: "producer_id",
            ..
        })
    ));
    assert!(at_max.is_ok());
}

#[test]
fn original_round_trips_by_id_within_its_session() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = new_session(&mut store);

    // Act
    let id = store.store_original(&session, "full tool output").unwrap();
    let loaded = store.load_original(&session, &id).unwrap();

    // Assert
    assert_eq!(loaded.as_deref(), Some("full tool output"));
}

#[test]
fn original_is_not_visible_from_another_session() {
    // Arrange
    let (_dir, mut store) = open_store();
    let owner = new_session(&mut store);
    let other = new_session(&mut store);
    let id = store.store_original(&owner, "private output").unwrap();

    // Act
    let loaded = store.load_original(&other, &id).unwrap();

    // Assert
    assert_eq!(loaded, None);
}

#[test]
fn unknown_original_id_loads_as_none() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = new_session(&mut store);

    // Act
    let by_number = store
        .load_original(&session, &OriginalId::new("999"))
        .unwrap();
    let by_garbage = store
        .load_original(&session, &OriginalId::new("nope"))
        .unwrap();

    // Assert
    assert_eq!((by_number, by_garbage), (None, None));
}

#[test]
fn oversize_original_is_rejected_and_the_cap_is_accepted() {
    // Arrange
    let (_dir, mut store) = open_store();
    let session = new_session(&mut store);
    let at_cap = "a".repeat(MAX_ORIGINAL_BYTES);
    let over_cap = "a".repeat(MAX_ORIGINAL_BYTES + 1);

    // Act
    let ok = store.store_original(&session, &at_cap);
    let rejected = store.store_original(&session, &over_cap);

    // Assert
    assert!(ok.is_ok());
    assert!(matches!(
        rejected,
        Err(StoreError::OriginalTooLarge { size, max })
            if size == MAX_ORIGINAL_BYTES + 1 && max == MAX_ORIGINAL_BYTES
    ));
}

#[test]
fn original_for_a_missing_session_is_rejected() {
    // Arrange
    let (_dir, mut store) = open_store();

    // Act
    let result = store.store_original(&SessionId::new("42"), "x");

    // Assert
    assert!(matches!(result, Err(StoreError::SessionNotFound { .. })));
}

#[test]
fn originals_reader_fetches_only_from_its_own_session() {
    // Arrange
    let (_dir, mut store) = open_store();
    let owner = new_session(&mut store);
    let other = new_session(&mut store);
    let id = store.store_original(&owner, "via trait").unwrap();
    let owner_reader = store.originals_reader(&owner);
    let other_reader = store.originals_reader(&other);
    let owner_view: &dyn OriginalsReader = &owner_reader;
    let other_view: &dyn OriginalsReader = &other_reader;

    // Act
    let found = owner_view.fetch_original(&id);
    let foreign = other_view.fetch_original(&id);
    let missing = owner_view.fetch_original(&OriginalId::new("777"));

    // Assert
    assert_eq!(found.unwrap(), "via trait");
    assert!(matches!(foreign, Err(ReaderError::NotFound(n)) if n == id.to_string()));
    assert!(matches!(missing, Err(ReaderError::NotFound(n)) if n == "777"));
}

#[test]
fn originals_over_the_session_cap_evict_the_oldest_first() {
    // Arrange
    let (_dir, mut store) = open_store();
    let owner = new_session(&mut store);
    let other = new_session(&mut store);
    let chunk = "x".repeat(MAX_ORIGINAL_BYTES);
    let kept_elsewhere = store.store_original(&other, &chunk).unwrap();
    let per_session = MAX_SESSION_ORIGINAL_BYTES / MAX_ORIGINAL_BYTES;
    let ids: Vec<OriginalId> = (0..=per_session)
        .map(|_| store.store_original(&owner, &chunk).unwrap())
        .collect();
    let reader = store.originals_reader(&owner);

    // Act
    let oldest = reader.fetch_original(&ids[0]);
    let second = reader.fetch_original(&ids[1]);
    let newest = reader.fetch_original(ids.last().unwrap());
    let elsewhere = store
        .originals_reader(&other)
        .fetch_original(&kept_elsewhere);

    // Assert
    assert!(matches!(oldest, Err(ReaderError::Evicted(n)) if n == ids[0].to_string()));
    assert_eq!(second.unwrap().len(), MAX_ORIGINAL_BYTES);
    assert_eq!(newest.unwrap().len(), MAX_ORIGINAL_BYTES);
    assert_eq!(elsewhere.unwrap().len(), MAX_ORIGINAL_BYTES);
}

#[test]
fn index_readers_only_see_their_own_repo() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .upsert_symbols("/repo-a", "src/lib.rs", vec![symbol("alpha_fn", "fn", 1)])
        .unwrap();
    store
        .upsert_symbols("/repo-b", "src/lib.rs", vec![symbol("beta_fn", "fn", 2)])
        .unwrap();
    let a = store.index_reader("/repo-a");
    let b = store.index_reader("/repo-b");
    let a_view: &dyn IndexReader = &a;
    let b_view: &dyn IndexReader = &b;

    // Act
    let a_outline = a_view.outline("src/lib.rs").unwrap();
    let b_outline = b_view.outline("src/lib.rs").unwrap();

    // Assert
    assert_eq!(a_outline.len(), 1);
    assert_eq!(a_outline[0].name, "alpha_fn");
    assert_eq!(b_outline.len(), 1);
    assert_eq!(b_outline[0].name, "beta_fn");
    assert_eq!(a_view.search("alpha_fn", 10).unwrap().len(), 1);
    assert_eq!(a_view.search("beta_fn", 10).unwrap(), Vec::new());
    assert_eq!(a_view.symbols("beta_fn").unwrap(), Vec::new());
    assert_eq!(b_view.search("alpha_fn", 10).unwrap(), Vec::new());
    assert_eq!(b_view.symbols("alpha_fn").unwrap(), Vec::new());
}

#[test]
fn index_reader_finds_a_written_symbol_by_search_and_symbols() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .upsert_symbols(
            "/repo",
            "src/lib.rs",
            vec![symbol("parse_config", "fn", 12)],
        )
        .unwrap();
    let bound = store.index_reader("/repo");
    let reader: &dyn IndexReader = &bound;

    // Act
    let hits = reader.search("parse_config", 10).unwrap();
    let symbols = reader.symbols("parse_config").unwrap();

    // Assert
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "src/lib.rs");
    assert_eq!(hits[0].line, 12);
    assert_eq!(hits[0].snippet, "parse_config");
    assert_eq!(
        symbols,
        vec![SymbolLocation {
            name: "parse_config".to_owned(),
            kind: "fn".to_owned(),
            path: "src/lib.rs".to_owned(),
            line: 12,
        }]
    );
}

#[test]
fn index_reader_outline_lists_a_file_in_source_order() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .upsert_symbols(
            "/repo",
            "src/a.rs",
            vec![symbol("second", "fn", 20), symbol("first", "struct", 3)],
        )
        .unwrap();
    store
        .upsert_symbols("/repo", "src/b.rs", vec![symbol("other", "fn", 1)])
        .unwrap();
    let bound = store.index_reader("/repo");
    let reader: &dyn IndexReader = &bound;

    // Act
    let outline = reader.outline("src/a.rs").unwrap();

    // Assert
    let names: Vec<_> = outline.iter().map(|s| (s.name.as_str(), s.line)).collect();
    assert_eq!(names, vec![("first", 3), ("second", 20)]);
}

#[test]
fn search_respects_the_limit_and_tolerates_query_syntax() {
    // Arrange
    let (_dir, mut store) = open_store();
    let symbols = (1..=5)
        .map(|n| symbol(&format!("item{n}"), "fn", n))
        .collect();
    store.upsert_symbols("/repo", "src/a.rs", symbols).unwrap();
    let bound = store.index_reader("/repo");
    let reader: &dyn IndexReader = &bound;

    // Act
    let limited = reader.search("item1", 1).unwrap();
    let odd = reader.search("\"AND (", 5);

    // Assert
    assert_eq!(limited.len(), 1);
    assert!(odd.is_ok());
}

#[test]
fn upserting_a_file_again_replaces_its_symbols() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .upsert_symbols("/repo", "src/a.rs", vec![symbol("old_name", "fn", 1)])
        .unwrap();

    // Act
    store
        .upsert_symbols("/repo", "src/a.rs", vec![symbol("new_name", "fn", 1)])
        .unwrap();

    // Assert
    let bound = store.index_reader("/repo");
    let reader: &dyn IndexReader = &bound;
    assert_eq!(reader.search("old_name", 10).unwrap(), Vec::new());
    assert_eq!(reader.search("new_name", 10).unwrap().len(), 1);
}

#[test]
fn clearing_a_file_removes_it_from_search_and_outline() {
    // Arrange
    let (_dir, mut store) = open_store();
    store
        .upsert_symbols("/repo", "src/a.rs", vec![symbol("gone_soon", "fn", 1)])
        .unwrap();
    store
        .upsert_symbols("/repo", "src/b.rs", vec![symbol("stays", "fn", 1)])
        .unwrap();

    // Act
    store.clear_file("/repo", "src/a.rs").unwrap();

    // Assert
    let bound = store.index_reader("/repo");
    let reader: &dyn IndexReader = &bound;
    assert_eq!(reader.search("gone_soon", 10).unwrap(), Vec::new());
    assert_eq!(reader.outline("src/a.rs").unwrap(), Vec::new());
    assert_eq!(reader.search("stays", 10).unwrap().len(), 1);
}
