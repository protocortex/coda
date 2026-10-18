// SPDX-License-Identifier: AGPL-3.0-only

//! Gitignore-aware workspace walk that indexes supported source files.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::lang::Language;
use crate::symbols::{Symbol, SymbolError, extract};

/// Default per-file size cap; larger files are skipped, not read.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;

/// A NUL byte within this many leading bytes marks a file as binary.
const BINARY_SNIFF_BYTES: usize = 8192;

/// Limits applied while walking.
#[derive(Debug, Clone, Copy)]
pub struct WalkLimits {
    pub max_file_bytes: u64,
}

impl Default for WalkLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

/// A source file and the symbols found in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    /// Path relative to the walk root.
    pub path: PathBuf,
    pub language: Language,
    pub symbols: Vec<Symbol>,
}

/// Why a supported file was left out of the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    TooLarge,
    Binary,
    Unreadable,
    ParseFailed,
}

/// A supported file that was skipped, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub path: PathBuf,
    pub reason: SkipReason,
}

/// The outcome of a walk.
#[derive(Debug, Default)]
pub struct WalkReport {
    pub files: Vec<IndexedFile>,
    pub skipped: Vec<Skipped>,
    /// Directory or entry errors hit while walking, as readable messages.
    pub errors: Vec<String>,
}

/// The walk could not start.
#[derive(Debug, thiserror::Error)]
pub enum WalkError {
    #[error("walk root {} is not a directory", .0.display())]
    NotADirectory(PathBuf),
    #[error("tree-sitter rejected the {0:?} grammar")]
    Grammar(Language),
}

/// Walks `root`, honoring ignore files, and extracts symbols from supported files.
///
/// # Errors
/// Returns [`WalkError`] if `root` is not a directory or a grammar cannot be
/// loaded. Problems with individual files are reported in
/// [`WalkReport::skipped`], and walk failures in [`WalkReport::errors`].
pub fn index_workspace(root: &Path, limits: WalkLimits) -> Result<WalkReport, WalkError> {
    walk(root, limits, extract)
}

type Extractor = fn(Language, &[u8]) -> Result<Vec<Symbol>, SymbolError>;

fn walk(root: &Path, limits: WalkLimits, extractor: Extractor) -> Result<WalkReport, WalkError> {
    if !root.is_dir() {
        return Err(WalkError::NotADirectory(root.to_path_buf()));
    }
    let mut report = WalkReport::default();
    // `require_git(false)` so .gitignore applies in unpacked or non-repo trees.
    let walker = WalkBuilder::new(root)
        .require_git(false)
        .sort_by_file_path(Ord::cmp)
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                report.errors.push(err.to_string());
                continue;
            }
        };
        let path = entry.path();
        let Some(language) = Language::from_path(path) else {
            continue;
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
        match index_file(path, language, limits, extractor) {
            Ok(symbols) => report.files.push(IndexedFile {
                path: rel,
                language,
                symbols,
            }),
            Err(FileFailure::Skip(reason)) => report.skipped.push(Skipped { path: rel, reason }),
            Err(FileFailure::Grammar(language)) => return Err(WalkError::Grammar(language)),
        }
    }
    Ok(report)
}

/// Why one file produced no symbols: skip it, or abort the whole walk.
enum FileFailure {
    Skip(SkipReason),
    Grammar(Language),
}

fn index_file(
    path: &Path,
    language: Language,
    limits: WalkLimits,
    extractor: Extractor,
) -> Result<Vec<Symbol>, FileFailure> {
    let bytes = read_capped(path, limits.max_file_bytes).map_err(FileFailure::Skip)?;
    if bytes.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0) {
        return Err(FileFailure::Skip(SkipReason::Binary));
    }
    extractor(language, &bytes).map_err(|err| match err {
        SymbolError::Grammar(language) => FileFailure::Grammar(language),
        SymbolError::Parse => FileFailure::Skip(SkipReason::ParseFailed),
    })
}

/// Reads at most `max + 1` bytes, so a file that grew past the cap after
/// being listed is detected without reading it whole.
fn read_capped(path: &Path, max: u64) -> Result<Vec<u8>, SkipReason> {
    let file = fs::File::open(path).map_err(|_| SkipReason::Unreadable)?;
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| SkipReason::Unreadable)?;
    if bytes.len() as u64 > max {
        return Err(SkipReason::TooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::symbols::SymbolError;

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, bytes).unwrap();
    }

    fn paths(report: &WalkReport) -> Vec<String> {
        report
            .files
            .iter()
            .map(|f| f.path.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn indexes_supported_files_with_symbols() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/a.rs", b"fn a() {}\n");
        write(dir.path(), "web/b.ts", b"class B {}\n");
        write(dir.path(), "notes.md", b"# hi\n");
        // Act
        let report = index_workspace(dir.path(), WalkLimits::default()).unwrap();
        // Assert
        assert_eq!(paths(&report), ["src/a.rs", "web/b.ts"]);
        assert_eq!(report.files[0].symbols[0].name, "a");
        assert_eq!(report.files[1].symbols[0].name, "B");
    }

    #[test]
    fn honors_gitignore_without_a_git_repo() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", b"ignored/\n");
        write(dir.path(), "ignored/x.rs", b"fn x() {}\n");
        write(dir.path(), "kept.rs", b"fn k() {}\n");
        // Act
        let report = index_workspace(dir.path(), WalkLimits::default()).unwrap();
        // Assert
        assert_eq!(paths(&report), ["kept.rs"]);
    }

    #[test]
    fn skips_files_over_the_size_cap() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "big.rs", b"fn big() {}\n");
        let limits = WalkLimits { max_file_bytes: 4 };
        // Act
        let report = index_workspace(dir.path(), limits).unwrap();
        // Assert
        assert_eq!(report.files, []);
        assert_eq!(
            report.skipped,
            [Skipped {
                path: "big.rs".into(),
                reason: SkipReason::TooLarge
            }]
        );
    }

    #[test]
    fn skips_binary_files() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bin.rs", b"fn a() {}\0\0\x01");
        // Act
        let report = index_workspace(dir.path(), WalkLimits::default()).unwrap();
        // Assert
        assert_eq!(report.files, []);
        assert_eq!(
            report.skipped,
            [Skipped {
                path: "bin.rs".into(),
                reason: SkipReason::Binary
            }]
        );
    }

    #[test]
    fn empty_and_broken_files_do_not_fail_the_run() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "empty.rs", b"");
        write(dir.path(), "broken.ts", b"function ok() {}\nclass {{{ (\n");
        write(dir.path(), "good.rs", b"fn g() {}\n");
        // Act
        let report = index_workspace(dir.path(), WalkLimits::default()).unwrap();
        // Assert
        assert_eq!(paths(&report), ["broken.ts", "empty.rs", "good.rs"]);
        assert_eq!(report.files[1].symbols, []);
        assert_eq!(report.skipped, []);
    }

    #[test]
    fn rejects_a_root_that_is_not_a_directory() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        // Act
        let err = index_workspace(&missing, WalkLimits::default()).unwrap_err();
        // Assert
        assert!(matches!(err, WalkError::NotADirectory(_)));
    }

    #[test]
    fn file_at_the_cap_is_indexed_and_one_byte_over_is_skipped() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let src = b"fn a() {}\n";
        write(dir.path(), "at.rs", src);
        write(dir.path(), "over.rs", b"fn ab() {}\n");
        let limits = WalkLimits {
            max_file_bytes: src.len() as u64,
        };
        // Act
        let report = index_workspace(dir.path(), limits).unwrap();
        // Assert
        assert_eq!(paths(&report), ["at.rs"]);
        assert_eq!(
            report.skipped,
            [Skipped {
                path: "over.rs".into(),
                reason: SkipReason::TooLarge
            }]
        );
    }

    #[test]
    fn read_capped_rejects_a_file_larger_than_the_cap() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.rs", b"0123456789");
        let path = dir.path().join("f.rs");
        // Act
        let at = read_capped(&path, 10);
        let over = read_capped(&path, 9);
        // Assert
        assert_eq!(at.unwrap(), b"0123456789");
        assert_eq!(over.unwrap_err(), SkipReason::TooLarge);
    }

    #[test]
    fn grammar_load_failure_aborts_the_walk_instead_of_skipping() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", b"fn a() {}\n");
        let broken: Extractor = |language, _| Err(SymbolError::Grammar(language));
        // Act
        let err = walk(dir.path(), WalkLimits::default(), broken).unwrap_err();
        // Assert
        assert!(matches!(err, WalkError::Grammar(Language::Rust)), "{err:?}");
    }

    #[test]
    fn parse_failure_skips_only_that_file() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.rs", b"fn a() {}\n");
        let aborted: Extractor = |_, _| Err(SymbolError::Parse);
        // Act
        let report = walk(dir.path(), WalkLimits::default(), aborted).unwrap();
        // Assert
        assert_eq!(
            report.skipped,
            [Skipped {
                path: "a.rs".into(),
                reason: SkipReason::ParseFailed
            }]
        );
    }

    #[cfg(unix)]
    mod unix {
        use std::os::unix::fs::{PermissionsExt, symlink};

        use super::*;

        #[test]
        fn unreadable_directory_is_recorded_and_the_rest_is_indexed() {
            // Arrange
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), "ok.rs", b"fn ok() {}\n");
            write(dir.path(), "locked/hidden.rs", b"fn h() {}\n");
            let locked = dir.path().join("locked");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
            if fs::read_dir(&locked).is_ok() {
                // Running as root: permissions are not enforced.
                fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
                return;
            }
            // Act
            let report = index_workspace(dir.path(), WalkLimits::default()).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
            // Assert
            assert_eq!(paths(&report), ["ok.rs"]);
            assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
            assert!(report.errors[0].contains("locked"), "{:?}", report.errors);
        }

        #[test]
        fn symlink_loop_is_not_followed() {
            // Arrange
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), "sub/a.rs", b"fn a() {}\n");
            symlink(dir.path(), dir.path().join("sub/loop")).unwrap();
            // Act
            let report = index_workspace(dir.path(), WalkLimits::default()).unwrap();
            // Assert
            assert_eq!(paths(&report), ["sub/a.rs"]);
            assert_eq!(report.errors, Vec::<String>::new());
        }

        #[test]
        fn symlinks_pointing_outside_the_root_are_not_followed() {
            // Arrange
            let outside = tempfile::tempdir().unwrap();
            write(outside.path(), "secret.rs", b"fn secret() {}\n");
            write(outside.path(), "dir/deep.rs", b"fn deep() {}\n");
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), "in.rs", b"fn inside() {}\n");
            symlink(outside.path().join("secret.rs"), dir.path().join("link.rs")).unwrap();
            symlink(outside.path().join("dir"), dir.path().join("linkdir")).unwrap();
            // Act
            let report = index_workspace(dir.path(), WalkLimits::default()).unwrap();
            // Assert
            assert_eq!(paths(&report), ["in.rs"]);
        }
    }
}
