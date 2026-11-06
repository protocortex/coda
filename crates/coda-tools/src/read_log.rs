// SPDX-License-Identifier: Apache-2.0

//! What a session has read, so `write` can tell a file it knows from one it does not.

use std::collections::HashMap;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// The identity of a file at one moment: the same value means the same bytes
/// for every practical purpose, the same test `edit` uses to detect a race.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl FileStamp {
    /// The stamp of the file `metadata` describes.
    #[must_use]
    pub fn of(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
        }
    }
}

/// The files a session has read, shared by the tools that record and check them.
#[derive(Debug, Clone, Default)]
pub struct ReadLog {
    stamps: Arc<Mutex<HashMap<PathBuf, FileStamp>>>,
}

impl ReadLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Notes that `path`, relative to the workspace, was seen as `stamp`.
    pub fn record(&self, path: &Path, stamp: FileStamp) {
        self.stamps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key(path), stamp);
    }

    /// The stamp `path` had when it was last recorded, if it ever was.
    #[must_use]
    pub fn stamp(&self, path: &Path) -> Option<FileStamp> {
        self.stamps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key(path))
            .copied()
    }
}

/// `path` without `.` components, so `./a` and `a` are one file.
fn key(path: &Path) -> PathBuf {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect()
}
