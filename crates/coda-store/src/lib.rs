// SPDX-License-Identifier: AGPL-3.0-only

//! SQLite persistence for coda. The only crate that opens the database.

mod decisions;
mod event_log;
mod events;
mod grants;
pub mod health;
mod index;
pub mod migrate;
mod originals;
mod pending;
mod readers;
mod savings;
mod sessions;
mod trust;
mod usage;

pub use decisions::{NewDecision, StoredDecision};
pub use event_log::{LogEvent, LogEventKind, LogScope, MAX_LOG_ID};
pub use events::{EventRecord, StoredEvent};
pub use grants::{Grant, GrantKey};
pub use index::{IndexStatus, IndexedFileState, Symbol};
pub use originals::{MAX_ORIGINAL_BYTES, MAX_SESSION_ORIGINAL_BYTES};
pub use pending::{NewPending, PendingPermission};
pub use readers::{RepoIndex, SessionOriginals};
pub use savings::SavingRecord;
pub use sessions::{NewSession, SessionFilter, SessionRecord, SessionSummary};
pub use usage::{UsageRecord, UsageSummary};

use std::fs::{self, DirBuilder, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use coda_protocol::types::{OriginalId, RequestId, SessionId, TurnId};
use rusqlite::{Connection, ErrorCode};

/// Schema version this binary reads and writes.
pub const SCHEMA_VERSION: u32 = 10;

/// Release of this binary, recorded in the database each time it opens it.
pub const BINARY_RELEASE: &str = env!("CARGO_PKG_VERSION");

/// File name of the database inside the data directory.
pub const DB_FILE_NAME: &str = "coda.db";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const WAL_SWITCH_ATTEMPTS: u32 = 50;
const WAL_SWITCH_BACKOFF: Duration = Duration::from_millis(20);
const DATA_DIR_MODE: u32 = 0o700;
const DB_FILE_MODE: u32 = 0o600;
const GROUP_AND_OTHER_BITS: u32 = 0o077;

/// Failures opening, migrating or using the store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    #[error("database schema version {found} is newer than the supported version {supported}")]
    SchemaTooNew { found: u32, supported: u32 },
    #[error(
        "database was last opened by release {found}, newer than this binary's {supported}; \
         upgrade coda, the database was left unchanged"
    )]
    ReleaseTooNew { found: String, supported: String },
    #[error(
        "migration step {seq} claims release {introduced_in}, newer than the binary's {binary}"
    )]
    StepFromFuture {
        seq: u32,
        introduced_in: String,
        binary: String,
    },
    #[error("migration step {seq} is out of order: steps must ascend by release and sequence")]
    StepsOutOfOrder { seq: u32 },
    #[error("migrations ended at schema version {found}, expected {expected}")]
    MigrationIncomplete { found: u32, expected: u32 },
    #[error("{value:?} is not a valid release version")]
    InvalidRelease { value: String },
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
    #[error("log field {field} value {value} exceeds the maximum {max}")]
    LogValueTooLarge {
        field: &'static str,
        value: u64,
        max: u64,
    },
    #[error("original of {size} bytes exceeds the {max} byte limit")]
    OriginalTooLarge { size: usize, max: usize },
    #[error("original {original} was evicted to keep its session within the storage limit")]
    OriginalEvicted { original: OriginalId },
    #[error("compressed size {compressed} exceeds original size {original}")]
    SavingExceedsOriginal { original: u64, compressed: u64 },
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
    /// enabled, [`StoreError::SchemaTooNew`] or
    /// [`StoreError::ReleaseTooNew`] when the database is newer than this binary, and I/O or SQLite errors otherwise.
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
        migrate::ensure_supported(migrate::read_version(&conn)?, SCHEMA_VERSION)?;
        migrate::ensure_release_supported(
            migrate::read_last_release(&conn)?.as_ref(),
            &migrate::binary_release()?,
        )?;
        let mode = switch_to_wal(&conn)?;
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
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// Switches the journal mode to WAL and returns the mode SQLite reports.
///
/// SQLite can return BUSY for this pragma without consulting the busy handler
/// when another connection is doing the same on a fresh file, so the busy
/// timeout alone does not cover it. The switch is idempotent, so retry it.
fn switch_to_wal(conn: &Connection) -> Result<String, StoreError> {
    let mut attempt = 1;
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0)) {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::DatabaseBusy && attempt < WAL_SWITCH_ATTEMPTS =>
            {
                attempt += 1;
                thread::sleep(WAL_SWITCH_BACKOFF);
            }
            result => return Ok(result?),
        }
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
