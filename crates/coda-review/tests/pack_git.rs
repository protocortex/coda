// SPDX-License-Identifier: Apache-2.0

//! The packer against a real `git diff`: packed line numbers must match the new files.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use coda_review::diff::{git_diff, parse_diff, tracked_files};
use coda_review::ignore::IgnoreSet;
use coda_review::pack::{PackOptions, main_extension, pack};

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "{args:?}");
}

fn numbered_lines(n: usize, tag: &str) -> String {
    let mut out = String::new();
    for i in 1..=n {
        writeln!(out, "{tag} line {i}").unwrap();
    }
    out
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    std::fs::write(p.join("a.rs"), numbered_lines(40, "a")).unwrap();
    std::fs::write(p.join("b.rs"), numbered_lines(120, "b")).unwrap();
    std::fs::write(p.join("gone.rs"), numbered_lines(5, "gone")).unwrap();
    std::fs::write(p.join("notes.md"), numbered_lines(5, "notes")).unwrap();
    git(p, &["add", "."]);
    git(p, &["commit", "-q", "-m", "base"]);
    git(p, &["checkout", "-q", "-b", "topic"]);
    let a = numbered_lines(40, "a")
        .replace("a line 5\n", "a line 5\nnew after 5\nnew after 5 again\n")
        .replace("a line 30\n", "");
    std::fs::write(p.join("a.rs"), a).unwrap();
    let b = numbered_lines(120, "b")
        .replace("b line 2\n", "b changed 2\n")
        .replace("b line 100\n", "b changed 100\nb added 100\n");
    std::fs::write(p.join("b.rs"), b).unwrap();
    std::fs::remove_file(p.join("gone.rs")).unwrap();
    std::fs::write(p.join("added.rs"), numbered_lines(3, "added")).unwrap();
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "topic"]);
    dir
}

#[test]
fn packed_line_numbers_match_the_new_files() {
    // Arrange
    let dir = repo();
    let diff = git_diff(dir.path(), "main", "topic").unwrap();
    let files = parse_diff(&diff);
    let tracked = tracked_files(dir.path()).unwrap();
    let main = main_extension(tracked.iter().map(String::as_str));
    let ignore = IgnoreSet::new(&[], true).unwrap();
    let options = PackOptions {
        budget: 100_000,
        ignore: &ignore,
        main_extension: main.as_deref(),
    };

    // Act
    let packed = pack(&files, options);

    // Assert
    let mut current: Option<String> = None;
    let mut checked = 0;
    for line in packed.text.lines() {
        if let Some(header) = line.strip_prefix("=== ") {
            let path = header.split(" (").next().unwrap().trim_end_matches(" ===");
            current = Some(path.to_owned());
            continue;
        }
        let Some(path) = &current else { continue };
        let Some((num, rest)) = line.trim_start().split_once(' ') else {
            continue;
        };
        let Ok(number) = num.parse::<usize>() else {
            continue;
        };
        let text = &rest[1..];
        let content = std::fs::read_to_string(dir.path().join(path)).unwrap();
        let actual = content.lines().nth(number - 1).unwrap();
        assert_eq!(actual, text, "{path}:{number}");
        checked += 1;
    }
    assert!(checked > 20, "only {checked} lines checked");
    assert!(packed.text.contains("- gone.rs\n"));
    assert_eq!(packed.omitted.len(), 1);
}

#[test]
fn tracked_files_returns_odd_names_exactly() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    let names = [
        "we ird\"q.txt",
        "tab\tname.txt",
        "back\\slash.txt",
        "caf\u{e9}.txt",
        "plain.rs",
    ];
    for name in names {
        std::fs::write(p.join(name), "x").unwrap();
    }
    git(p, &["add", "."]);

    // Act
    let mut tracked = tracked_files(p).unwrap();

    // Assert
    tracked.sort();
    let mut expected: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    expected.sort();
    assert_eq!(tracked, expected);
}
