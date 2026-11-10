// SPDX-License-Identifier: Apache-2.0

//! The outline of a repository: the definitions no other definition encloses.

use coda_store::{Store, Symbol};
use tempfile::TempDir;

const REPO: &str = "/repo";

fn symbol(name: &str, line: u32) -> Symbol {
    Symbol {
        name: name.to_owned(),
        kind: "fn".to_owned(),
        line,
        end_line: line + 2,
        parent: None,
    }
}

#[test]
fn top_level_symbols_leave_out_nested_ones_and_come_back_in_file_and_line_order() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let mut store = Store::open(&dir.path().join("data")).unwrap();
    let nested = Symbol {
        parent: Some("Outer".to_owned()),
        ..symbol("method", 3)
    };
    store
        .replace_file(
            REPO,
            "src/b.rs",
            "h",
            vec![symbol("second", 20), symbol("first", 2), nested],
        )
        .unwrap();
    store
        .replace_file(REPO, "src/a.rs", "h", vec![symbol("alpha", 1)])
        .unwrap();
    store
        .replace_file("/other", "src/z.rs", "h", vec![symbol("zeta", 1)])
        .unwrap();

    // Act
    let found = store.top_level_symbols(REPO).unwrap();

    // Assert
    let names: Vec<(&str, &str)> = found
        .iter()
        .map(|s| (s.path.as_str(), s.name.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            ("src/a.rs", "alpha"),
            ("src/b.rs", "first"),
            ("src/b.rs", "second")
        ]
    );
    assert_eq!((found[1].line, found[1].end_line), (2, 4));
}
