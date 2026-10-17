// SPDX-License-Identifier: AGPL-3.0-only

//! Workspace-confined file access that cannot be redirected by symlinks.

use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use coda_protocol::paths::{Target, classify_target};
use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, fstat, openat, renameat, statat, unlinkat,
};
use rustix::io::Errno;

const CURRENT_DIR: &str = ".";
const PARENT_DIR: &str = "..";

/// Why the sandbox refused or failed to open a path.
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// The path is absolute; tools take workspace-relative paths only.
    #[error("path {0} is absolute; use a path relative to the workspace")]
    Absolute(String),
    /// The path contains a `..` component.
    #[error("path {0} contains '..'")]
    ParentComponent(String),
    /// The path resolves outside the workspace root.
    #[error("path {0} is outside the workspace")]
    OutsideRoot(String),
    /// A component of the path is a symbolic link.
    #[error("path {0} passes through a symbolic link")]
    Symlink(String),
    /// A directory was expected but something else was found.
    #[error("path {0} is not a directory")]
    NotADirectory(String),
    /// A regular file was expected but something else was found.
    #[error("path {0} is not a regular file")]
    NotARegularFile(String),
    /// The path does not exist.
    #[error("path {0} does not exist")]
    NotFound(String),
    /// Any other operating system error.
    #[error("opening {path} failed: {source}")]
    Io {
        /// The workspace-relative path being opened.
        path: String,
        /// The underlying error.
        source: io::Error,
    },
}

/// The kind of a directory entry, without following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// Anything else, including symlinks.
    Other,
}

/// One name in a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry's file name.
    pub name: OsString,
    /// What the entry is.
    pub kind: EntryKind,
}

/// An open directory inside the sandbox.
#[derive(Debug)]
pub struct SandboxDir {
    fd: OwnedFd,
    display: String,
}

impl SandboxDir {
    /// Lists the directory, sorted by name.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Io`] when the listing fails.
    pub fn entries(&self) -> Result<Vec<DirEntry>, SandboxError> {
        let io_err = |source: Errno| SandboxError::Io {
            path: self.display.clone(),
            source: source.into(),
        };
        let mut entries = Vec::new();
        for item in Dir::read_from(&self.fd).map_err(io_err)? {
            let item = item.map_err(io_err)?;
            let name = OsStr::from_bytes(item.file_name().to_bytes());
            if name == CURRENT_DIR || name == PARENT_DIR {
                continue;
            }
            let file_type = match item.file_type() {
                FileType::Unknown => statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map(|stat| FileType::from_raw_mode(stat.st_mode))
                    .map_err(io_err)?,
                known => known,
            };
            entries.push(DirEntry {
                name: name.to_os_string(),
                kind: match file_type {
                    FileType::RegularFile => EntryKind::File,
                    FileType::Directory => EntryKind::Directory,
                    _ => EntryKind::Other,
                },
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Opens the regular file `name` directly inside this directory.
    ///
    /// # Errors
    ///
    /// Fails when `name` is a symlink, is not a regular file, or cannot be opened.
    pub fn open_file(&self, name: &OsStr) -> Result<File, SandboxError> {
        let display = join_display(&self.display, name);
        open_child(&self.fd, name, &display, Want::File).map(File::from)
    }

    /// Creates a new file `name` in this directory, failing if it exists.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Io`] when the file cannot be created.
    pub fn create_new(&self, name: &OsStr, mode: Mode) -> Result<File, SandboxError> {
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        openat(&self.fd, name, flags, mode)
            .map(File::from)
            .map_err(|errno| self.io_error(name, errno))
    }

    /// Renames `from` over `to`, both directly inside this directory.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Io`] when the rename fails.
    pub fn rename(&self, from: &OsStr, to: &OsStr) -> Result<(), SandboxError> {
        renameat(&self.fd, from, &self.fd, to).map_err(|errno| self.io_error(to, errno))
    }

    /// Removes the file `name` directly inside this directory.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Io`] when the removal fails.
    pub fn remove(&self, name: &OsStr) -> Result<(), SandboxError> {
        unlinkat(&self.fd, name, AtFlags::empty()).map_err(|errno| self.io_error(name, errno))
    }

    fn io_error(&self, name: &OsStr, errno: Errno) -> SandboxError {
        SandboxError::Io {
            path: join_display(&self.display, name),
            source: errno.into(),
        }
    }

    /// Opens the subdirectory `name` directly inside this directory.
    ///
    /// # Errors
    ///
    /// Fails when `name` is a symlink, is not a directory, or cannot be opened.
    pub fn open_dir(&self, name: &OsStr) -> Result<SandboxDir, SandboxError> {
        let display = join_display(&self.display, name);
        let fd = open_child(&self.fd, name, &display, Want::Directory)?;
        Ok(SandboxDir { fd, display })
    }
}

/// A workspace root that tools open files through.
///
/// Paths are checked with `classify_target`, then opened one component at a
/// time with `O_NOFOLLOW`, so a symlink swapped in after the check is refused.
#[derive(Clone)]
pub struct Sandbox {
    root: PathBuf,
    before_open: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Sandbox {
    /// Creates a sandbox over `root`.
    ///
    /// # Errors
    ///
    /// Returns [`SandboxError::Io`] when `root` cannot be resolved.
    pub fn new(root: &Path) -> Result<Self, SandboxError> {
        let root = fs::canonicalize(root).map_err(|source| SandboxError::Io {
            path: root.display().to_string(),
            source,
        })?;
        Ok(Self {
            root,
            before_open: None,
        })
    }

    /// Runs `hook` between the path check and the open, so tests can swap a
    /// path for a symlink at the exact moment a race would.
    #[must_use]
    pub fn with_before_open(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.before_open = Some(Arc::new(hook));
        self
    }

    /// Opens a regular file at the workspace-relative `rel`.
    ///
    /// # Errors
    ///
    /// Fails when the path is absolute, contains `..`, leaves the workspace,
    /// passes through a symlink, or is not a regular file.
    pub fn open_file(&self, rel: &Path) -> Result<File, SandboxError> {
        let (parent, file_name) = self.open_parent(rel)?;
        parent.open_file(&file_name)
    }

    /// Opens the directory holding the workspace-relative file `rel`.
    ///
    /// Returns the directory and the file's name inside it, so a caller can
    /// create or replace siblings without re-resolving the path.
    ///
    /// # Errors
    ///
    /// Same conditions as [`Sandbox::open_file`], except the file itself is
    /// not opened.
    pub fn open_parent(&self, rel: &Path) -> Result<(SandboxDir, OsString), SandboxError> {
        let mut names = self.checked_components(rel)?;
        let Some(file_name) = names.pop() else {
            return Err(SandboxError::NotARegularFile(rel.display().to_string()));
        };
        let parent_display = names.iter().collect::<PathBuf>().display().to_string();
        Ok((self.descend(&names, &parent_display)?, file_name))
    }

    /// Opens the directory at the workspace-relative `rel`.
    ///
    /// # Errors
    ///
    /// Same conditions as [`Sandbox::open_file`], but the target must be a directory.
    pub fn open_dir(&self, rel: &Path) -> Result<SandboxDir, SandboxError> {
        let display = rel.display().to_string();
        let names = self.checked_components(rel)?;
        self.descend(&names, &display)
    }

    /// Validates `rel`, classifies it against the root and runs the test hook.
    fn checked_components(&self, rel: &Path) -> Result<Vec<OsString>, SandboxError> {
        let display = || rel.display().to_string();
        let mut names = Vec::new();
        for component in rel.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    return Err(SandboxError::Absolute(display()));
                }
                Component::ParentDir => return Err(SandboxError::ParentComponent(display())),
                Component::CurDir => {}
                Component::Normal(name) => names.push(name.to_os_string()),
            }
        }
        if classify_target(&self.root, rel) != Target::Inside {
            return Err(SandboxError::OutsideRoot(display()));
        }
        if let Some(hook) = &self.before_open {
            hook();
        }
        Ok(names)
    }

    fn descend(&self, names: &[OsString], display: &str) -> Result<SandboxDir, SandboxError> {
        let root_fd = openat(
            CWD,
            &self.root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|source| SandboxError::Io {
            path: self.root.display().to_string(),
            source: source.into(),
        })?;
        let mut dir = SandboxDir {
            fd: root_fd,
            display: String::new(),
        };
        for name in names {
            dir = dir.open_dir(name)?;
        }
        display.clone_into(&mut dir.display);
        Ok(dir)
    }
}

#[derive(Clone, Copy)]
enum Want {
    File,
    Directory,
}

fn join_display(parent: &str, name: &OsStr) -> String {
    Path::new(parent).join(name).display().to_string()
}

/// Opens `name` under `dir` without following a symlink at that component.
fn open_child(
    dir: &OwnedFd,
    name: &OsStr,
    display: &str,
    want: Want,
) -> Result<OwnedFd, SandboxError> {
    // NONBLOCK keeps a swapped-in FIFO from hanging the open; the type check below rejects it.
    let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    if matches!(want, Want::Directory) {
        flags |= OFlags::DIRECTORY;
    }
    let fd = openat(dir, name, flags, Mode::empty()).map_err(|errno| {
        let not_wanted = || match want {
            Want::File => SandboxError::NotARegularFile(display.to_owned()),
            Want::Directory => SandboxError::NotADirectory(display.to_owned()),
        };
        match errno {
            Errno::LOOP => SandboxError::Symlink(display.to_owned()),
            Errno::NOENT => SandboxError::NotFound(display.to_owned()),
            Errno::NOTDIR => match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
                    SandboxError::Symlink(display.to_owned())
                }
                _ => not_wanted(),
            },
            other => SandboxError::Io {
                path: display.to_owned(),
                source: other.into(),
            },
        }
    })?;
    if matches!(want, Want::File) {
        let stat = fstat(&fd).map_err(|errno| SandboxError::Io {
            path: display.to_owned(),
            source: errno.into(),
        })?;
        if !FileType::from_raw_mode(stat.st_mode).is_file() {
            return Err(SandboxError::NotARegularFile(display.to_owned()));
        }
    }
    Ok(fd)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

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
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("sub/file.txt"), "inside").unwrap();
        fs::write(root.join("b.txt"), "b").unwrap();
        fs::write(root.join("a.txt"), "a").unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        Fixture {
            _dir: dir,
            root,
            outside,
        }
    }

    fn read_all(mut file: File) -> String {
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn opens_regular_file_inside_root() {
        // Arrange
        let f = fixture();
        let sandbox = Sandbox::new(&f.root).unwrap();

        // Act
        let file = sandbox.open_file(Path::new("sub/file.txt")).unwrap();

        // Assert
        assert_eq!(read_all(file), "inside");
    }

    #[test]
    fn rejects_absolute_path_even_inside_root() {
        // Arrange
        let f = fixture();
        let sandbox = Sandbox::new(&f.root).unwrap();

        // Act
        let result = sandbox.open_file(&f.root.join("a.txt"));

        // Assert
        assert!(matches!(result, Err(SandboxError::Absolute(_))));
    }

    #[test]
    fn rejects_parent_component_even_when_it_stays_inside() {
        // Arrange
        let f = fixture();
        let sandbox = Sandbox::new(&f.root).unwrap();

        // Act
        let result = sandbox.open_file(Path::new("sub/../a.txt"));

        // Assert
        assert!(matches!(result, Err(SandboxError::ParentComponent(_))));
    }

    #[test]
    fn rejects_symlinked_directory_inside_root() {
        // Arrange
        let f = fixture();
        symlink(f.root.join("sub"), f.root.join("link")).unwrap();
        let sandbox = Sandbox::new(&f.root).unwrap();

        // Act
        let result = sandbox.open_file(Path::new("link/file.txt"));

        // Assert
        assert!(matches!(result, Err(SandboxError::Symlink(_))));
    }

    #[test]
    fn rejects_symlinked_file_pointing_inside_root() {
        // Arrange
        let f = fixture();
        symlink(f.root.join("a.txt"), f.root.join("alias.txt")).unwrap();
        let sandbox = Sandbox::new(&f.root).unwrap();

        // Act
        let result = sandbox.open_file(Path::new("alias.txt"));

        // Assert
        assert!(matches!(result, Err(SandboxError::Symlink(_))));
    }

    #[test]
    fn rejects_symlink_escaping_the_root() {
        // Arrange
        let f = fixture();
        symlink(f.outside.join("secret.txt"), f.root.join("leak.txt")).unwrap();
        let sandbox = Sandbox::new(&f.root).unwrap();

        // Act
        let result = sandbox.open_file(Path::new("leak.txt"));

        // Assert
        assert!(matches!(result, Err(SandboxError::OutsideRoot(_))));
    }

    #[test]
    fn rejects_file_swapped_for_symlink_between_check_and_open() {
        // Arrange
        let f = fixture();
        let target = f.root.join("sub/file.txt");
        let secret = f.outside.join("secret.txt");
        let sandbox = Sandbox::new(&f.root).unwrap().with_before_open(move || {
            fs::remove_file(&target).unwrap();
            symlink(&secret, &target).unwrap();
        });

        // Act
        let result = sandbox.open_file(Path::new("sub/file.txt"));

        // Assert
        assert!(matches!(result, Err(SandboxError::Symlink(_))));
    }

    #[test]
    fn rejects_directory_swapped_for_symlink_between_check_and_open() {
        // Arrange
        let f = fixture();
        fs::write(f.outside.join("file.txt"), "evil").unwrap();
        let dir = f.root.join("sub");
        let outside = f.outside.clone();
        let sandbox = Sandbox::new(&f.root).unwrap().with_before_open(move || {
            fs::remove_dir_all(&dir).unwrap();
            symlink(&outside, &dir).unwrap();
        });

        // Act
        let result = sandbox.open_file(Path::new("sub/file.txt"));

        // Assert
        assert!(matches!(result, Err(SandboxError::Symlink(_))));
    }

    #[test]
    fn missing_file_is_not_found() {
        // Arrange
        let f = fixture();
        let sandbox = Sandbox::new(&f.root).unwrap();

        // Act
        let result = sandbox.open_file(Path::new("sub/nope.txt"));

        // Assert
        assert!(matches!(result, Err(SandboxError::NotFound(_))));
    }

    #[test]
    fn directory_is_not_a_regular_file() {
        // Arrange
        let f = fixture();
        let sandbox = Sandbox::new(&f.root).unwrap();

        // Act
        let result = sandbox.open_file(Path::new("sub"));

        // Assert
        assert!(matches!(result, Err(SandboxError::NotARegularFile(_))));
    }

    #[test]
    fn lists_entries_sorted_with_kinds() {
        // Arrange
        let f = fixture();
        symlink(f.root.join("a.txt"), f.root.join("c-link")).unwrap();
        let sandbox = Sandbox::new(&f.root).unwrap();

        // Act
        let entries = sandbox.open_dir(Path::new(".")).unwrap().entries().unwrap();

        // Assert
        let listed: Vec<_> = entries
            .iter()
            .map(|e| (e.name.to_str().unwrap(), e.kind))
            .collect();
        assert_eq!(
            listed,
            [
                ("a.txt", EntryKind::File),
                ("b.txt", EntryKind::File),
                ("c-link", EntryKind::Other),
                ("sub", EntryKind::Directory),
            ]
        );
    }
}
