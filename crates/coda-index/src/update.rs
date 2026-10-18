// SPDX-License-Identifier: AGPL-3.0-only

//! Incremental population of the symbol index through the store.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use coda_store::{Store, StoreError, Symbol as StoredSymbol};

use crate::symbols::Symbol;
use crate::walk::{
    IndexedFile, SkipReason, Skipped, WalkError, WalkLimits, WalkReport, index_workspace,
};

/// A file the store refused to update; its previous rows are untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedFile {
    /// Path relative to the repo root.
    pub path: String,
    pub reason: String,
}

/// What one [`update_index`] run did, with paths relative to the repo root.
#[derive(Debug, Default)]
pub struct UpdateReport {
    /// Files written because they were new or their content changed.
    pub indexed: Vec<String>,
    /// Files whose content hash matched the stored one.
    pub unchanged: Vec<String>,
    /// Files no longer on disk whose rows were cleared.
    pub removed: Vec<String>,
    /// Files the walk left out. Unreadable and unparsable ones keep their old
    /// rows; too large and binary ones are cleared.
    pub skipped: Vec<Skipped>,
    pub failed: Vec<FailedFile>,
    /// Walk errors, such as an unreadable directory. Pruning is skipped
    /// while any are present, since the files below it may still exist.
    pub walk_errors: Vec<String>,
}

/// The update could not run at all.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("could not walk the repo: {0}")]
    Walk(#[from] WalkError),
    #[error("could not read the stored index state: {0}")]
    Store(#[from] StoreError),
}

/// Walks `root` and applies the result with [`apply_walk`].
///
/// # Errors
///
/// Returns [`UpdateError`] when the walk cannot start or [`apply_walk`] fails.
pub fn update_index(
    store: &mut Store,
    root: &Path,
    limits: WalkLimits,
) -> Result<UpdateReport, UpdateError> {
    apply_walk(store, root, index_workspace(root, limits)?)
}

/// Brings the index of the repo at `root` in line with a finished `walk`.
///
/// Taking a finished walk lets a caller walk without holding the store.
///
/// The repo is keyed by `root` as given. A file whose content hash is
/// unchanged keeps its rows and has its index time renewed, a changed one has
/// its rows replaced in one transaction, and one no longer on disk is cleared.
/// A file the walk skipped or the store rejected keeps its previous rows,
/// except one that is too large or binary, which is cleared.
///
/// # Errors
///
/// Returns [`UpdateError`] when the stored state cannot be read or renewed. A
/// failure on one file is reported in [`UpdateReport::failed`].
pub fn apply_walk(
    store: &mut Store,
    root: &Path,
    walk: WalkReport,
) -> Result<UpdateReport, UpdateError> {
    let repo = root.to_string_lossy();
    let stored: BTreeMap<String, String> = store
        .indexed_files(&repo)?
        .into_iter()
        .map(|state| (state.file_path, state.content_hash))
        .collect();

    let mut report = UpdateReport::default();
    let mut present = BTreeSet::new();
    for file in walk.files {
        let path = file.path.to_string_lossy().into_owned();
        if stored.get(&path) == Some(&file.content_hash) {
            report.unchanged.push(path.clone());
        } else {
            write_file(store, &repo, &path, file, &mut report);
        }
        present.insert(path);
    }
    // Too large or binary is a property of the content, so those rows go
    // stale for good. Unreadable or unparsable files may recover; keep theirs.
    let mut dropped = BTreeSet::new();
    for skipped in &walk.skipped {
        let path = skipped.path.to_string_lossy().into_owned();
        if matches!(skipped.reason, SkipReason::TooLarge | SkipReason::Binary) {
            dropped.insert(path);
        } else {
            present.insert(path);
        }
    }
    report.skipped = walk.skipped;
    report.walk_errors = walk.errors;

    // An unreadable directory hides files that may still exist, so absence
    // from the walk proves nothing and only content-based drops are safe.
    let prune_absent = report.walk_errors.is_empty();
    let stale = stored
        .keys()
        .filter(|path| !present.contains(*path) && (prune_absent || dropped.contains(*path)));
    for path in stale {
        match store.clear_file(&repo, path) {
            Ok(()) => report.removed.push(path.clone()),
            Err(err) => report.failed.push(failure(path, &err)),
        }
    }
    store.restamp_files(&repo, &report.unchanged)?;
    Ok(report)
}

fn write_file(
    store: &mut Store,
    repo: &str,
    path: &str,
    file: IndexedFile,
    report: &mut UpdateReport,
) {
    let symbols = file.symbols.into_iter().map(to_stored).collect();
    match store.replace_file(repo, path, &file.content_hash, symbols) {
        Ok(()) => report.indexed.push(path.to_owned()),
        Err(err) => report.failed.push(failure(path, &err)),
    }
}

fn failure(path: &str, err: &StoreError) -> FailedFile {
    FailedFile {
        path: path.to_owned(),
        reason: err.to_string(),
    }
}

fn to_stored(symbol: Symbol) -> StoredSymbol {
    StoredSymbol {
        name: symbol.name,
        kind: symbol.kind.as_str().to_owned(),
        line: saturating_u32(symbol.line),
        end_line: saturating_u32(symbol.end_line),
        parent: symbol.parent,
    }
}

fn saturating_u32(line: usize) -> u32 {
    u32::try_from(line).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use coda_protocol::readers::IndexReader;
    use coda_store::{Clock, Store};
    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;
    use crate::walk::WalkLimits;

    #[derive(Debug, Default)]
    struct TestClock(AtomicI64);

    impl Clock for TestClock {
        fn now_ms(&self) -> i64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    struct Fixture {
        _data: TempDir,
        db: std::path::PathBuf,
        store: Store,
        clock: Arc<TestClock>,
        repo: TempDir,
    }

    fn fixture() -> Fixture {
        let data = TempDir::new().unwrap();
        let clock = Arc::new(TestClock::default());
        let db = data.path().join("data");
        let store = Store::open_with_clock(&db, clock.clone()).unwrap();
        Fixture {
            _data: data,
            db,
            store,
            clock,
            repo: TempDir::new().unwrap(),
        }
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn run(f: &mut Fixture) -> UpdateReport {
        update_index(&mut f.store, f.repo.path(), WalkLimits::default()).unwrap()
    }

    fn find(store: &Store, root: &Path, query: &str) -> Vec<(String, u32)> {
        let reader = store.index_reader(&root.to_string_lossy());
        let view: &dyn IndexReader = &reader;
        view.search(query, 10)
            .unwrap()
            .into_iter()
            .map(|h| (h.path, h.line))
            .collect()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn first_run_indexes_every_supported_file() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "src/a.rs", "fn alpha() {}\n");
        write(f.repo.path(), "web/b.ts", "class Beta {}\n");

        // Act
        let report = run(&mut f);

        // Assert
        assert_eq!(report.indexed, strings(&["src/a.rs", "web/b.ts"]));
        assert_eq!(
            find(&f.store, f.repo.path(), "alpha"),
            [("src/a.rs".to_owned(), 1)]
        );
        assert_eq!(
            find(&f.store, f.repo.path(), "Beta"),
            [("web/b.ts".to_owned(), 1)]
        );
    }

    #[test]
    fn unchanged_files_are_skipped_by_content_hash_and_restamped() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "src/a.rs", "fn alpha() {}\n");
        f.clock.0.store(100, Ordering::SeqCst);
        run(&mut f);
        f.clock.0.store(900, Ordering::SeqCst);

        // Act
        let report = run(&mut f);

        // Assert
        assert_eq!(report.indexed.len(), 0);
        assert_eq!(report.unchanged, strings(&["src/a.rs"]));
        let files = f
            .store
            .indexed_files(&f.repo.path().to_string_lossy())
            .unwrap();
        assert_eq!(files[0].indexed_at, 900);
    }

    #[test]
    fn a_modified_file_replaces_its_rows() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "src/a.rs", "fn old_name() {}\n");
        run(&mut f);
        write(f.repo.path(), "src/a.rs", "\n\nfn new_name() {}\n");

        // Act
        let report = run(&mut f);

        // Assert
        assert_eq!(report.indexed, strings(&["src/a.rs"]));
        assert_eq!(find(&f.store, f.repo.path(), "old_name").len(), 0);
        assert_eq!(
            find(&f.store, f.repo.path(), "new_name"),
            [("src/a.rs".to_owned(), 3)]
        );
    }

    #[test]
    fn a_finished_walk_applies_without_touching_the_disk_again() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "src/a.rs", "fn walked() {}\n");
        let walk = index_workspace(f.repo.path(), WalkLimits::default()).unwrap();
        fs::remove_file(f.repo.path().join("src/a.rs")).unwrap();

        // Act
        let report = apply_walk(&mut f.store, f.repo.path(), walk).unwrap();

        // Assert
        assert_eq!(report.indexed, strings(&["src/a.rs"]));
        assert_eq!(find(&f.store, f.repo.path(), "walked").len(), 1);
    }

    #[test]
    fn a_deleted_file_is_removed_from_the_index() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "src/a.rs", "fn doomed() {}\n");
        write(f.repo.path(), "src/b.rs", "fn survivor() {}\n");
        run(&mut f);
        fs::remove_file(f.repo.path().join("src/a.rs")).unwrap();

        // Act
        let report = run(&mut f);

        // Assert
        assert_eq!(report.removed, strings(&["src/a.rs"]));
        assert_eq!(find(&f.store, f.repo.path(), "doomed").len(), 0);
        assert_eq!(find(&f.store, f.repo.path(), "survivor").len(), 1);
        let files = f
            .store
            .indexed_files(&f.repo.path().to_string_lossy())
            .unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn a_file_the_store_rejects_is_reported_and_keeps_its_old_rows() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "src/a.rs", "fn steady() {}\n");
        write(f.repo.path(), "src/b.rs", "fn fine() {}\n");
        run(&mut f);
        Connection::open(f.db.join("coda.db"))
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER poison BEFORE INSERT ON symbols WHEN new.name = 'poison' \
                 BEGIN SELECT RAISE(ABORT, 'rejected'); END;",
            )
            .unwrap();
        write(
            f.repo.path(),
            "src/a.rs",
            "fn steady() {}\nfn poison() {}\n",
        );
        write(f.repo.path(), "src/b.rs", "fn fine() {}\nfn extra() {}\n");

        // Act
        let report = run(&mut f);

        // Assert
        assert_eq!(report.indexed, strings(&["src/b.rs"]));
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].path, "src/a.rs");
        assert_eq!(find(&f.store, f.repo.path(), "steady").len(), 1);
        assert_eq!(find(&f.store, f.repo.path(), "extra").len(), 1);
    }

    #[test]
    fn an_unreadable_file_keeps_its_rows_and_is_not_treated_as_deleted() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "src/a.rs", "fn lingering() {}\n");
        run(&mut f);
        let path = f.repo.path().join("src/a.rs");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        // Act
        let report = run(&mut f);

        // Assert
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(report.removed.len(), 0);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(find(&f.store, f.repo.path(), "lingering").len(), 1);
    }

    #[test]
    fn an_unreadable_directory_keeps_its_subtree_and_is_reported() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "locked/a.rs", "fn hidden() {}\n");
        write(f.repo.path(), "open/b.rs", "fn visible() {}\n");
        run(&mut f);
        let dir = f.repo.path().join("locked");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&dir).is_ok() {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            return; // running as root: permissions are not enforced
        }

        // Act
        let report = run(&mut f);

        // Assert
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(report.removed.len(), 0);
        assert_eq!(report.walk_errors.len(), 1);
        assert_eq!(find(&f.store, f.repo.path(), "hidden").len(), 1);
    }

    #[test]
    fn a_file_that_became_too_large_has_its_rows_cleared() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "src/a.rs", "fn bulky() {}\n");
        run(&mut f);
        write(
            f.repo.path(),
            "src/a.rs",
            "fn bulky() {}\n// padding padding\n",
        );
        let limits = WalkLimits { max_file_bytes: 16 };

        // Act
        let report = update_index(&mut f.store, f.repo.path(), limits).unwrap();

        // Assert
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.removed, strings(&["src/a.rs"]));
        assert_eq!(find(&f.store, f.repo.path(), "bulky").len(), 0);
    }

    #[test]
    fn a_file_that_became_binary_has_its_rows_cleared() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "src/a.rs", "fn blobby() {}\n");
        run(&mut f);
        fs::write(f.repo.path().join("src/a.rs"), b"fn blobby() {}\0\n").unwrap();

        // Act
        let report = run(&mut f);

        // Assert
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.removed, strings(&["src/a.rs"]));
        assert_eq!(find(&f.store, f.repo.path(), "blobby").len(), 0);
    }

    #[test]
    fn repos_are_indexed_and_pruned_independently() {
        // Arrange
        let mut f = fixture();
        let other = TempDir::new().unwrap();
        write(f.repo.path(), "src/a.rs", "fn shared() {}\n");
        write(other.path(), "src/a.rs", "fn shared() {}\n");
        run(&mut f);
        update_index(&mut f.store, other.path(), WalkLimits::default()).unwrap();
        fs::remove_file(f.repo.path().join("src/a.rs")).unwrap();

        // Act
        run(&mut f);

        // Assert
        assert_eq!(find(&f.store, f.repo.path(), "shared").len(), 0);
        assert_eq!(find(&f.store, other.path(), "shared").len(), 1);
    }

    #[test]
    fn status_shows_when_the_repo_was_last_indexed() {
        // Arrange
        let mut f = fixture();
        write(f.repo.path(), "src/a.rs", "fn alpha() {}\nfn beta() {}\n");
        f.clock.0.store(4_242, Ordering::SeqCst);

        // Act
        run(&mut f);

        // Assert
        let status = f
            .store
            .index_status(&f.repo.path().to_string_lossy())
            .unwrap();
        assert_eq!((status.files, status.symbols), (1, 2));
        assert_eq!(status.last_indexed_at, Some(4_242));
    }

    #[test]
    fn a_missing_root_is_an_error() {
        // Arrange
        let mut f = fixture();
        let missing = f.repo.path().join("nope");

        // Act
        let result = update_index(&mut f.store, &missing, WalkLimits::default());

        // Assert
        assert!(matches!(result, Err(UpdateError::Walk(_))));
    }
}
