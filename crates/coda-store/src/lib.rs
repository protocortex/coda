// SPDX-License-Identifier: AGPL-3.0-only

//! SQLite persistence for coda. The only crate that opens the database.

mod events;
pub mod migrate;
mod sessions;

pub use events::{EventRecord, StoredEvent};
pub use sessions::{NewSession, SessionFilter, SessionRecord};

use std::fs::{self, DirBuilder, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use coda_protocol::types::{RequestId, SessionId, TurnId};
use rusqlite::Connection;

/// Schema version this binary reads and writes.
pub const SCHEMA_VERSION: u32 = 2;

const DB_FILE_NAME: &str = "coda.db";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DATA_DIR_MODE: u32 = 0o700;
const DB_FILE_MODE: u32 = 0o600;
const GROUP_AND_OTHER_BITS: u32 = 0o077;

/// Failures opening, migrating or using the store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    #[error("session {session} does not exist")]
    SessionNotFound { session: SessionId },
    #[error("turn {turn} does not exist in session {session}")]
    TurnNotFound { session: SessionId, turn: TurnId },
    #[error("event {seq} already exists in session {session}")]
    DuplicateAppend { session: SessionId, seq: u64 },
    #[error("session {session} already has another writer")]
    WriterBusy { session: SessionId },
    #[error("pending permission {request} does not exist")]
    PendingNotFound { request: RequestId },
    #[error("pending permission {request} is already resolved")]
    AlreadyResolved { request: RequestId },
    #[error("value {value} does not fit the database integer range")]
    ValueOutOfRange { value: u64 },
    #[error("stored value could not be encoded or decoded: {0}")]
    Encoding(#[from] serde_json::Error),
    #[error("data directory I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Source of timestamps, injected so tests are deterministic.
pub trait Clock: Send + Sync + std::fmt::Debug {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

/// The wall clock.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
    }
}

static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);

/// An open, migrated coda database.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
    clock: Arc<dyn Clock>,
    /// Canonical database path, part of the writer lease key.
    db_path: PathBuf,
    /// Identifies this handle as the owner of its writer leases.
    id: u64,
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
        Self::open_with_clock(data_dir, Arc::new(SystemClock))
    }

    /// Like [`Store::open`], taking timestamps from `clock`.
    ///
    /// # Errors
    ///
    /// Fails in the same ways as [`Store::open`].
    pub fn open_with_clock(data_dir: &Path, clock: Arc<dyn Clock>) -> Result<Self, StoreError> {
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
        Ok(Self {
            conn,
            clock,
            db_path: fs::canonicalize(&db_path)?,
            id: NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed),
        })
    }

    /// The underlying connection.
    #[allow(dead_code)]
    pub(crate) fn connection(&self) -> &Connection {
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
