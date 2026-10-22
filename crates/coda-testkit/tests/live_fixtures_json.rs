// SPDX-License-Identifier: AGPL-3.0-only

//! Every committed live fixture must stay valid JSON, line by line.

use std::fs;
use std::path::{Path, PathBuf};

fn live_fixtures(provider: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(provider);
    let mut files: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "jsonl")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("live-"))
        })
        .collect();
    files.sort();
    files
}

#[test]
fn every_committed_live_fixture_line_is_valid_json() {
    // Arrange
    let files: Vec<_> = ["claude", "codex"]
        .into_iter()
        .flat_map(live_fixtures)
        .collect();

    // Act
    let invalid: Vec<String> = files
        .iter()
        .flat_map(|file| {
            fs::read_to_string(file)
                .unwrap()
                .lines()
                .enumerate()
                .filter(|(_, line)| serde_json::from_str::<serde_json::Value>(line).is_err())
                .map(|(i, _)| format!("{}:{}", file.display(), i + 1))
                .collect::<Vec<_>>()
        })
        .collect();

    // Assert
    assert_eq!(files.len(), 8, "expected eight live fixtures");
    assert!(invalid.is_empty(), "invalid JSON lines: {invalid:?}");
}
