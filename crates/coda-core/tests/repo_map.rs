// SPDX-License-Identifier: Apache-2.0

//! The repository map built from a real index of a real folder.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use coda_config::repo_map::RepoMapConfig;
use coda_core::repo_map::{HEADER, build};
use coda_core::tools::refresh_index;
use coda_store::Store;
use tempfile::TempDir;

const WINDOW: u32 = 200_000;
const LATER: Duration = Duration::from_secs(3600);

struct Rig {
    _dir: TempDir,
    store: Arc<Mutex<Store>>,
    root: PathBuf,
}

impl Rig {
    fn new(files: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir(&root).unwrap();
        for (path, text) in files {
            write(&root, path, text);
        }
        let store = Arc::new(Mutex::new(Store::open(&dir.path().join("data")).unwrap()));
        refresh_index(&store, &root);
        Self {
            _dir: dir,
            store,
            root,
        }
    }

    fn map(&self, message: &str, window: Option<u32>, config: &RepoMapConfig) -> Option<String> {
        build(&self.store, &self.root, message, window, config, Vec::new())
    }
}

fn write(root: &Path, path: &str, text: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

#[test]
fn the_map_lists_top_level_definitions_and_ranks_a_file_the_message_names_first() {
    // Arrange
    let rig = Rig::new(&[
        ("src/a.rs", "pub fn alpha() {}\n"),
        (
            "src/b.rs",
            "pub struct Beta;\nimpl Beta {\n    fn inner(&self) {}\n}\n",
        ),
    ]);

    // Act
    let map = rig
        .map(
            "change b.rs please",
            Some(WINDOW),
            &RepoMapConfig::default(),
        )
        .unwrap();

    // Assert
    assert!(map.starts_with(HEADER), "{map}");
    let (a, b) = (map.find("src/a.rs").unwrap(), map.find("src/b.rs").unwrap());
    assert!(b < a, "{map}");
    assert!(map.contains("alpha"), "{map}");
    assert!(!map.contains("inner"), "{map}");
}

#[test]
fn an_unchanged_repository_gives_the_same_map_every_time() {
    // Arrange
    let rig = Rig::new(&[("a.rs", "fn one() {}\n"), ("b.rs", "fn two() {}\n")]);
    let config = RepoMapConfig::default();

    // Act
    let first = rig.map("hello", Some(WINDOW), &config);
    let second = rig.map("hello", Some(WINDOW), &config);

    // Assert
    assert!(first.is_some());
    assert_eq!(first, second);
}

#[test]
fn the_map_stays_inside_the_share_of_the_window() {
    // Arrange
    let many: Vec<(String, String)> = (0..200)
        .map(|i| {
            (
                format!("src/m{i:03}.rs"),
                format!("fn function_number_{i}() {{}}\n"),
            )
        })
        .collect();
    let files: Vec<(&str, &str)> = many.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let rig = Rig::new(&files);
    let config = RepoMapConfig {
        enabled: true,
        token_share_percent: 1,
    };
    let window = 20_000;

    // Act
    let map = rig.map("", Some(window), &config).unwrap();

    // Assert
    assert!(
        map.len() <= usize::try_from(window).unwrap() / 100 * 4,
        "{}",
        map.len()
    );
    assert!(map.contains("more files not shown"), "{map}");
}

#[test]
fn an_edit_after_indexing_is_said_in_the_map() {
    // Arrange
    let rig = Rig::new(&[("a.rs", "fn one() {}\n")]);
    write(&rig.root, "a.rs", "fn one() {}\nfn two() {}\n");
    // A coarse file clock can stamp the edit before the index time, so move it ahead.
    std::fs::File::options()
        .write(true)
        .open(rig.root.join("a.rs"))
        .unwrap()
        .set_modified(SystemTime::now() + LATER)
        .unwrap();

    // Act
    let map = rig
        .map("", Some(WINDOW), &RepoMapConfig::default())
        .unwrap();

    // Assert
    assert!(map.contains("index: stale"), "{map}");
}

#[test]
fn an_empty_or_unsupported_folder_gives_no_map() {
    // Arrange
    let empty = Rig::new(&[]);
    let prose = Rig::new(&[("README.md", "# Notes\n"), ("data.txt", "text\n")]);
    let config = RepoMapConfig::default();

    // Act / Assert
    assert_eq!(empty.map("", Some(WINDOW), &config), None);
    assert_eq!(prose.map("", Some(WINDOW), &config), None);
}

#[test]
fn no_map_when_it_is_off_or_the_window_is_unknown() {
    // Arrange
    let rig = Rig::new(&[("a.rs", "fn one() {}\n")]);
    let off = RepoMapConfig {
        enabled: false,
        ..RepoMapConfig::default()
    };

    // Act / Assert
    assert_eq!(rig.map("", Some(WINDOW), &off), None);
    assert_eq!(rig.map("", None, &RepoMapConfig::default()), None);
}
