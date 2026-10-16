// SPDX-License-Identifier: AGPL-3.0-only

//! Workspace path classification and repository identity.

use std::fs;
use std::path::{Component, Path, PathBuf};

const GIT_ENTRY: &str = ".git";

/// Where a path lands relative to a workspace root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Resolves to the root or somewhere beneath it.
    Inside,
    /// Names a location elsewhere and does not claim to be in the root.
    Outside,
    /// Looks like it is in the root but cannot be resolved to a place inside
    /// it: a symlink escape, or an error such as permission denied.
    Escapes,
}

/// Classifies `path` against `workspace_root`.
///
/// A relative `path` is taken as relative to the root. `..` is applied to the
/// already resolved prefix, as the operating system does, and symlinks of
/// existing components are followed. Components that do not exist yet are
/// normalized lexically, so the check also works for files about to be created.
///
/// # Security
///
/// The answer holds only at check time: the filesystem can change before the
/// path is used (TOCTOU). Callers must re-check, or open with `O_NOFOLLOW` or
/// `openat` relative to the root at use time.
#[must_use]
pub fn classify_target(workspace_root: &Path, path: &Path) -> Target {
    let Some(root) = resolve(workspace_root) else {
        return Target::Outside;
    };
    let joined = workspace_root.join(path);
    if resolve(&joined).is_some_and(|resolved| resolved.starts_with(&root)) {
        return Target::Inside;
    }
    let claims_inside =
        path.is_relative() || path.starts_with(workspace_root) || path.starts_with(&root);
    if claims_inside {
        Target::Escapes
    } else {
        Target::Outside
    }
}

/// Resolves `path` to an absolute form with no `.`, `..` or symlinks.
///
/// Returns `None` when the path cannot be resolved (a symlink escape target
/// that does not exist, or an error such as permission denied), so callers
/// fail closed.
fn resolve(path: &Path) -> Option<PathBuf> {
    let mut resolved = PathBuf::new();
    // Components pushed after the first missing one; once `..` pops them all,
    // `resolved` is a canonical existing prefix again and symlinks matter.
    let mut missing_depth = 0_usize;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
                missing_depth = missing_depth.saturating_sub(1);
            }
            Component::Normal(name) => {
                resolved.push(name);
                if missing_depth > 0 {
                    missing_depth += 1;
                    continue;
                }
                match fs::canonicalize(&resolved) {
                    Ok(canonical) => resolved = canonical,
                    Err(_) if fs::symlink_metadata(&resolved).is_ok() => return None,
                    Err(_) => missing_depth = 1,
                }
            }
        }
    }
    Some(resolved)
}

/// Finds the canonical root of the git repository that contains `cwd`.
///
/// Walks up looking for `.git`, a directory or, in linked worktrees, a file.
/// Returns `None` when `cwd` does not exist or no ancestor is a repository.
#[must_use]
pub fn repo_identity(cwd: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(cwd).ok()?;
    canonical
        .ancestors()
        .find(|dir| dir.join(GIT_ENTRY).exists())
        .map(Path::to_path_buf)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    /// A workspace root with a sibling directory outside it.
    struct Fixture {
        _dir: TempDir,
        root: PathBuf,
        outside: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(outside.join("dir")).unwrap();
        fs::write(root.join("sub/file.txt"), "x").unwrap();
        fs::write(outside.join("secret.txt"), "x").unwrap();
        Fixture {
            _dir: dir,
            root,
            outside,
        }
    }

    #[test]
    fn existing_relative_path_is_inside() {
        // Arrange
        let f = fixture();

        // Act
        let target = classify_target(&f.root, Path::new("sub/file.txt"));

        // Assert
        assert_eq!(target, Target::Inside);
    }

    #[test]
    fn non_existing_tail_is_inside() {
        // Arrange
        let f = fixture();

        // Act
        let target = classify_target(&f.root, Path::new("sub/new/deeper/file.txt"));

        // Assert
        assert_eq!(target, Target::Inside);
    }

    #[test]
    fn root_equals_path_is_inside() {
        // Arrange
        let f = fixture();

        // Act
        let target = classify_target(&f.root, &f.root);

        // Assert
        assert_eq!(target, Target::Inside);
    }

    #[test]
    fn dot_dot_that_stays_inside_is_inside() {
        // Arrange
        let f = fixture();

        // Act
        let target = classify_target(&f.root, Path::new("sub/../sub/./file.txt"));

        // Assert
        assert_eq!(target, Target::Inside);
    }

    #[test]
    fn dot_dot_that_leaves_the_root_escapes() {
        // Arrange
        let f = fixture();

        // Act
        let relative = classify_target(&f.root, Path::new("../outside/secret.txt"));
        let nested = classify_target(&f.root, Path::new("sub/../../outside/secret.txt"));
        let absolute = classify_target(&f.root, &f.root.join("../outside/secret.txt"));

        // Assert
        assert_eq!(relative, Target::Escapes);
        assert_eq!(nested, Target::Escapes);
        assert_eq!(absolute, Target::Escapes);
    }

    #[test]
    fn absolute_path_elsewhere_is_outside() {
        // Arrange
        let f = fixture();

        // Act
        let target = classify_target(&f.root, &f.outside.join("secret.txt"));

        // Assert
        assert_eq!(target, Target::Outside);
    }

    #[test]
    fn absolute_path_under_root_is_inside() {
        // Arrange
        let f = fixture();

        // Act
        let target = classify_target(&f.root, &f.root.join("sub/file.txt"));

        // Assert
        assert_eq!(target, Target::Inside);
    }

    #[test]
    fn symlink_to_inside_is_inside() {
        // Arrange
        let f = fixture();
        symlink(f.root.join("sub"), f.root.join("alias")).unwrap();

        // Act
        let target = classify_target(&f.root, Path::new("alias/file.txt"));

        // Assert
        assert_eq!(target, Target::Inside);
    }

    #[test]
    fn symlink_to_outside_escapes() {
        // Arrange
        let f = fixture();
        symlink(&f.outside, f.root.join("link")).unwrap();

        // Act
        let relative = classify_target(&f.root, Path::new("link/secret.txt"));
        let absolute = classify_target(&f.root, &f.root.join("link/secret.txt"));

        // Assert
        assert_eq!(relative, Target::Escapes);
        assert_eq!(absolute, Target::Escapes);
    }

    #[test]
    fn dot_dot_after_an_outside_symlink_is_resolved_from_the_link_target() {
        // Arrange
        let f = fixture();
        symlink(f.outside.join("dir"), f.root.join("link")).unwrap();

        // Act
        let target = classify_target(&f.root, Path::new("link/../secret.txt"));

        // Assert
        assert_eq!(target, Target::Escapes);
    }

    #[test]
    fn dot_dot_after_a_missing_component_does_not_skip_a_symlink() {
        // Arrange
        let f = fixture();
        symlink(&f.outside, f.root.join("link")).unwrap();

        // Act
        let target = classify_target(&f.root, Path::new("missing/../link/secret.txt"));

        // Assert
        assert_eq!(target, Target::Escapes);
    }

    #[test]
    fn dot_dot_after_a_missing_component_stays_inside_without_symlinks() {
        // Arrange
        let f = fixture();

        // Act
        let target = classify_target(&f.root, Path::new("missing/../inside.txt"));

        // Assert
        assert_eq!(target, Target::Inside);
    }

    #[test]
    fn dangling_symlink_to_outside_escapes() {
        // Arrange
        let f = fixture();
        symlink(f.outside.join("missing"), f.root.join("dangling")).unwrap();

        // Act
        let target = classify_target(&f.root, Path::new("dangling"));

        // Assert
        assert_eq!(target, Target::Escapes);
    }

    #[test]
    fn repo_identity_finds_git_dir_from_a_nested_cwd() {
        // Arrange
        let f = fixture();
        fs::create_dir(f.root.join(".git")).unwrap();

        // Act
        let identity = repo_identity(&f.root.join("sub"));

        // Assert
        assert_eq!(identity, Some(fs::canonicalize(&f.root).unwrap()));
    }

    #[test]
    fn repo_identity_accepts_a_git_file_from_a_worktree() {
        // Arrange
        let f = fixture();
        fs::write(f.root.join(".git"), "gitdir: /elsewhere").unwrap();

        // Act
        let identity = repo_identity(&f.root);

        // Assert
        assert_eq!(identity, Some(fs::canonicalize(&f.root).unwrap()));
    }

    #[test]
    fn repo_identity_is_canonical_through_a_symlinked_cwd() {
        // Arrange
        let f = fixture();
        fs::create_dir(f.root.join(".git")).unwrap();
        let alias = f.outside.join("alias");
        symlink(f.root.join("sub"), &alias).unwrap();

        // Act
        let identity = repo_identity(&alias);

        // Assert
        assert_eq!(identity, Some(fs::canonicalize(&f.root).unwrap()));
    }

    #[test]
    fn repo_identity_is_none_without_a_git_root() {
        // Arrange
        let f = fixture();

        // Act
        let identity = repo_identity(&f.root);

        // Assert
        assert_eq!(identity, None);
    }

    #[test]
    fn repo_identity_is_none_for_a_missing_cwd() {
        // Arrange
        let f = fixture();

        // Act
        let identity = repo_identity(&f.root.join("missing"));

        // Assert
        assert_eq!(identity, None);
    }
}
