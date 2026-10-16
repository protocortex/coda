// SPDX-License-Identifier: AGPL-3.0-only

//! SQLite persistence for coda. The only crate that opens the database.

pub mod migrate;

use std::fs::{self, DirBuilder, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;

/// Schema version this binary reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

const DB_FILE_NAME: &str = "coda.db";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DATA_DIR_MODE: u32 = 0o700;
const DB_FILE_MODE: u32 = 0o600;
const GROUP_AND_OTHER_BITS: u32 = 0o077;

/// Failures opening or migrating the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database schema version {found} is newer than the supported version {supported}")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error("data directory {} is accessible by other users, expected mode 0700", path.display())]
    InsecureDataDir { path: PathBuf },
    #[error("data directory {} is a symbolic link, refusing to follow it", path.display())]
    SymlinkedDataDir { path: PathBuf },
    #[error("database file {} is accessible by other users, expected mode 0600", path.display())]
    InsecureDbFile { path: PathBuf },
    #[error("could not enable WAL journal mode, SQLite reported {mode:?}")]
    WalNotEnabled { mode: String },
    #[error("data directory I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// An open, migrated coda database.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens `coda.db` under `data_dir`, creating the directory (mode 0700)
    /// and file (mode 0600) when missing, then enables WAL and foreign keys
    /// and applies pending migrations.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InsecureDataDir`] when an existing directory is
    /// open to group or others, [`StoreError::SymlinkedDataDir`] when it is a
    /// symlink, [`StoreError::InsecureDbFile`] when an existing database file is
    /// open to group or others, [`StoreError::WalNotEnabled`] when WAL cannot be
    /// enabled, [`StoreError::SchemaTooNew`] when the database
    /// is newer than this binary, and I/O or SQLite errors otherwise.
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        prepare_data_dir(data_dir)?;
        let db_path = data_dir.join(DB_FILE_NAME);
        check_existing_db_file(&db_path)?;
        // Created first so the umask cannot loosen it; SQLite gives the WAL
        // and shared-memory files the same mode as the database file.
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(DB_FILE_MODE)
            .open(&db_path)?;

        let mut conn = Connection::open(&db_path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Checked before switching to WAL so a refused database is left untouched.
        migrate::ensure_supported(migrate::read_version(&conn)?)?;
        let mode: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::WalNotEnabled { mode });
        }
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// The underlying connection.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

fn prepare_data_dir(data_dir: &Path) -> Result<(), StoreError> {
    if let Ok(meta) = fs::symlink_metadata(data_dir) {
        if meta.file_type().is_symlink() {
            return Err(StoreError::SymlinkedDataDir {
                path: data_dir.to_path_buf(),
            });
        }
        let mode = meta.permissions().mode();
        if mode & GROUP_AND_OTHER_BITS != 0 {
            return Err(StoreError::InsecureDataDir {
                path: data_dir.to_path_buf(),
            });
        }
        return Ok(());
    }
    DirBuilder::new()
        .recursive(true)
        .mode(DATA_DIR_MODE)
        .create(data_dir)?;
    Ok(())
}

fn check_existing_db_file(db_path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(db_path) {
        Ok(meta) if meta.permissions().mode() & GROUP_AND_OTHER_BITS != 0 => {
            Err(StoreError::InsecureDbFile {
                path: db_path.to_path_buf(),
            })
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}
