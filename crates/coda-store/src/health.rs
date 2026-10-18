// SPDX-License-Identifier: AGPL-3.0-only

//! Read-only inspection of the database file, for `coda doctor`. Unlike
//! [`crate::Store::open`] it never creates, migrates or switches the journal
//! mode, so it can look at a database this binary could not open.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::{DB_FILE_NAME, StoreError, migrate};

/// Most integrity problems collected; a badly damaged file reports many.
const MAX_PROBLEMS: usize = 10;
const INTEGRITY_OK: &str = "ok";

/// Opens the database without writing next to it. Even a read-only open of a
/// WAL database creates its `-wal` and `-shm` files, so a database with no
/// leftover WAL is opened immutable instead, which reads the main file alone.
/// One with a WAL left by a crashed daemon is opened read-only so those
/// changes are seen.
fn open_read_only(data_dir: &Path) -> Result<Option<Connection>, StoreError> {
    let db_path = data_dir.join(DB_FILE_NAME);
    if !db_path.is_file() {
        return Ok(None);
    }
    if data_dir.join(format!("{DB_FILE_NAME}-wal")).exists() {
        let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        return Ok(Some(conn));
    }
    let escaped = db_path
        .to_string_lossy()
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(format!("file:{escaped}?immutable=1"), flags)?;
    Ok(Some(conn))
}

/// The schema version recorded in the database, or `None` when there is no file.
///
/// # Errors
/// Returns [`StoreError::Sqlite`] when the file is not a readable database.
pub fn schema_version(data_dir: &Path) -> Result<Option<u32>, StoreError> {
    open_read_only(data_dir)?
        .map(|conn| migrate::read_version(&conn))
        .transpose()
}

/// The problems SQLite's `integrity_check` finds (empty when the database is
/// sound), or `None` when there is no file.
///
/// # Errors
/// Returns [`StoreError::Sqlite`] when the file is not a readable database.
pub fn integrity_problems(data_dir: &Path) -> Result<Option<Vec<String>>, StoreError> {
    let Some(conn) = open_read_only(data_dir)? else {
        return Ok(None);
    };
    let mut statement = conn.prepare("PRAGMA integrity_check")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut problems = Vec::new();
    for row in rows.take(MAX_PROBLEMS) {
        let line = row?;
        if line != INTEGRITY_OK {
            problems.push(line);
        }
    }
    Ok(Some(problems))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SCHEMA_VERSION, Store};

    #[test]
    fn a_fresh_store_is_current_and_sound() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        drop(Store::open(&data).unwrap());

        // Act
        let version = schema_version(&data).unwrap();
        let problems = integrity_problems(&data).unwrap();

        // Assert
        assert_eq!(version, Some(SCHEMA_VERSION));
        assert_eq!(problems, Some(Vec::new()));
    }

    #[test]
    fn a_file_that_is_not_a_database_is_an_error() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(DB_FILE_NAME), vec![b'x'; 4096]).unwrap();

        // Act
        let result = integrity_problems(dir.path());

        // Assert
        assert!(matches!(result, Err(StoreError::Sqlite(_))));
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let meta = e.metadata().unwrap();
                format!("{:?} {} {:?}", e.file_name(), meta.len(), meta.modified())
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn inspecting_creates_and_modifies_no_file_and_a_missing_database_has_no_answers() {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        drop(Store::open(&data).unwrap());
        let missing = dir.path().join("missing");
        std::fs::create_dir(&missing).unwrap();
        for inspected in [&data, &missing] {
            let before = listing(inspected);

            // Act
            let version = schema_version(inspected).unwrap();
            let problems = integrity_problems(inspected).unwrap();

            // Assert
            assert_eq!(listing(inspected), before);
            assert_eq!(version.is_none(), problems.is_none());
        }
    }
}
