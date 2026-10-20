// SPDX-License-Identifier: AGPL-3.0-only

//! Ordered migrations, each tied to the release that introduced it.
//!
//! Every data-changing step and one-time data fix is a [`Step`] with a
//! sequence number (mirrored in `PRAGMA user_version`) and the release it
//! first shipped in, so every migration is attached to a release. Which steps
//! run is decided by sequence against `user_version`, never by release, so a
//! step added within an already-shipped version is not skipped. The `meta`
//! table records the release that last opened the database, and an open
//! refuses a database written by a newer release rather than guess at its
//! layout.
//!
//! State-based crash cleanup (a stale socket, orphaned children, turns left
//! interrupted) is not versioned: it inspects current state on every start and
//! so is not a migration.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use semver::Version;

use crate::{BINARY_RELEASE, StoreError};

const LAST_RELEASE_KEY: &str = "last_release";

/// One migration: SQL that runs once, in the release that introduced it.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    /// Sequence number, stored in `PRAGMA user_version` once applied.
    pub seq: u32,
    /// Semver of the first release that ships this step.
    pub introduced_in: &'static str,
    pub sql: &'static str,
}

/// Every step, sorted by `(introduced_in, seq)`. The steps that predate the
/// first release are tagged 0.0.0; later ones carry the release that ships them.
pub const STEPS: &[Step] = &[
    Step {
        seq: 1,
        introduced_in: "0.0.0",
        sql: include_str!("schema.sql"),
    },
    Step {
        seq: 2,
        introduced_in: "0.0.0",
        sql: include_str!("schema_v2.sql"),
    },
    Step {
        seq: 3,
        introduced_in: "0.0.0",
        sql: include_str!("schema_v3.sql"),
    },
    Step {
        seq: 4,
        introduced_in: "0.0.0",
        sql: include_str!("schema_v4.sql"),
    },
    Step {
        seq: 5,
        introduced_in: "0.0.0",
        sql: include_str!("schema_v5.sql"),
    },
    Step {
        seq: 6,
        introduced_in: "0.0.0",
        sql: include_str!("schema_v6.sql"),
    },
    Step {
        seq: 7,
        introduced_in: "0.0.0",
        sql: include_str!("schema_v7.sql"),
    },
    Step {
        seq: 8,
        introduced_in: "0.0.0",
        sql: include_str!("schema_v8.sql"),
    },
];

/// Brings `conn` up to the current schema and records this binary's release.
/// Running it on an up-to-date database changes nothing.
///
/// # Errors
///
/// See [`migrate_with`].
pub fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    migrate_with(conn, STEPS, BINARY_RELEASE)
}

/// Runs the `steps` above the database's `user_version`, each with its version
/// bump in one transaction, then records `binary_release`.
///
/// What runs is decided by sequence alone, so a step added within a release
/// still reaches databases already stamped with it. The release gates the open
/// instead: a database stamped newer than `binary_release` is refused before
/// any write, and the stamp is written with the last applied step (or on its
/// own when no step ran and the stamp is stale or absent). Releases compare
/// without build metadata.
///
/// # Errors
///
/// Returns [`StoreError::StepFromFuture`] when a step claims a release newer
/// than `binary_release`, [`StoreError::StepsOutOfOrder`] when `steps` is not
/// ascending by `(introduced_in, seq)`, [`StoreError::ReleaseTooNew`] when the
/// database was last opened by a newer release, [`StoreError::SchemaTooNew`]
/// when its schema is ahead of `steps`, and [`StoreError::Sqlite`] when a step
/// fails, in which case that step is rolled back and the recorded release is
/// kept.
pub fn migrate_with(
    conn: &mut Connection,
    steps: &[Step],
    binary_release: &str,
) -> Result<(), StoreError> {
    let binary = parse_release(binary_release)?;
    validate_steps(steps, &binary, binary_release)?;
    let supported = steps.last().map_or(0, |s| s.seq);

    for step in steps {
        // IMMEDIATE takes the write lock before the state is read, so a
        // racing opener waits and then sees the step already applied.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version = read_version(&tx)?;
        ensure_supported(version, supported)?;
        ensure_release_supported(read_last_release(&tx)?.as_ref(), &binary)?;
        if version >= step.seq {
            continue;
        }
        tx.execute_batch(step.sql)?;
        tx.pragma_update(None, "user_version", step.seq)?;
        if step.seq == supported {
            record_release(&tx, &binary)?;
        }
        tx.commit()?;
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version = read_version(&tx)?;
    if version != supported {
        return Err(StoreError::MigrationIncomplete {
            found: version,
            expected: supported,
        });
    }
    let recorded = read_last_release(&tx)?;
    ensure_release_supported(recorded.as_ref(), &binary)?;
    if recorded.as_ref() != Some(&binary) {
        record_release(&tx, &binary)?;
    }
    tx.commit()?;
    Ok(())
}

/// Checks that no step is from a release newer than the binary and that the
/// list ascends by `(introduced_in, seq)` with strictly rising sequences.
fn validate_steps(
    steps: &[Step],
    binary: &Version,
    binary_release: &str,
) -> Result<(), StoreError> {
    let mut previous: Option<(Version, u32)> = None;
    for step in steps {
        let introduced = parse_release(step.introduced_in)?;
        if introduced > *binary {
            return Err(StoreError::StepFromFuture {
                seq: step.seq,
                introduced_in: step.introduced_in.to_owned(),
                binary: binary_release.to_owned(),
            });
        }
        if let Some((release, seq)) = &previous
            && (introduced < *release || step.seq <= *seq)
        {
            return Err(StoreError::StepsOutOfOrder { seq: step.seq });
        }
        previous = Some((introduced, step.seq));
    }
    Ok(())
}

fn record_release(conn: &Connection, release: &Version) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        (LAST_RELEASE_KEY, release.to_string()),
    )?;
    Ok(())
}

/// Reads `PRAGMA user_version`.
pub(crate) fn read_version(conn: &Connection) -> Result<u32, StoreError> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Refuses a database whose schema is ahead of `supported`.
pub(crate) fn ensure_supported(found: u32, supported: u32) -> Result<(), StoreError> {
    if found > supported {
        return Err(StoreError::SchemaTooNew { found, supported });
    }
    Ok(())
}

/// Refuses a database last opened by a release newer than the binary.
pub(crate) fn ensure_release_supported(
    recorded: Option<&Version>,
    binary: &Version,
) -> Result<(), StoreError> {
    match recorded {
        Some(found) if found > binary => Err(StoreError::ReleaseTooNew {
            found: found.to_string(),
            supported: binary.to_string(),
        }),
        _ => Ok(()),
    }
}

/// The release recorded by the last open, or `None` for a database that
/// predates the `meta` table (or has not recorded one yet).
pub(crate) fn read_last_release(conn: &Connection) -> Result<Option<Version>, StoreError> {
    let has_meta: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
        [],
        |r| r.get(0),
    )?;
    if !has_meta {
        return Ok(None);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [LAST_RELEASE_KEY],
            |r| r.get(0),
        )
        .optional()?;
    value.as_deref().map(parse_release).transpose()
}

/// This binary's release as a version.
pub(crate) fn binary_release() -> Result<Version, StoreError> {
    parse_release(BINARY_RELEASE)
}

/// Parses a release, dropping build metadata: semver orders it, but two builds
/// of one version are the same release.
fn parse_release(text: &str) -> Result<Version, StoreError> {
    let mut version = Version::parse(text).map_err(|_| StoreError::InvalidRelease {
        value: text.to_owned(),
    })?;
    version.build = semver::BuildMetadata::EMPTY;
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BINARY: &str = "1.0.0";

    fn last_release(conn: &Connection) -> Option<String> {
        read_last_release(conn).unwrap().map(|v| v.to_string())
    }

    fn user_version(conn: &Connection) -> u32 {
        read_version(conn).unwrap()
    }

    fn step(seq: u32, introduced_in: &'static str, sql: &'static str) -> Step {
        Step {
            seq,
            introduced_in,
            sql,
        }
    }

    /// The shipped steps followed by `extra`.
    fn with_extra(extra: &[Step]) -> Vec<Step> {
        let mut all: Vec<Step> = STEPS.to_vec();
        all.extend_from_slice(extra);
        all
    }

    fn count_table() -> &'static str {
        "CREATE TABLE IF NOT EXISTS ran(n INTEGER); INSERT INTO ran VALUES (1);"
    }

    fn ran(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM ran", [], |r| r.get(0))
            .unwrap_or(0)
    }

    #[test]
    fn shipped_steps_are_sorted_and_none_is_newer_than_the_binary() {
        // Arrange
        let binary = Version::parse(env!("CARGO_PKG_VERSION")).unwrap();

        // Act
        let keys: Vec<_> = STEPS
            .iter()
            .map(|s| (Version::parse(s.introduced_in).unwrap(), s.seq))
            .collect();

        // Assert
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{keys:?}");
        assert!(keys.iter().all(|(release, _)| *release <= binary));
        assert_eq!(STEPS.last().map(|s| s.seq), Some(crate::SCHEMA_VERSION));
    }

    #[test]
    fn a_fresh_database_records_the_binary_release() {
        // Arrange
        let mut conn = Connection::open_in_memory().unwrap();

        // Act
        migrate(&mut conn).unwrap();

        // Assert
        assert_eq!(
            last_release(&conn).as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn a_pre_meta_database_migrates_forward_and_records_the_release() {
        // Arrange
        let mut conn = Connection::open_in_memory().unwrap();
        for s in STEPS.iter().filter(|s| s.seq <= 7) {
            conn.execute_batch(s.sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 7).unwrap();
        assert_eq!(last_release(&conn), None);

        // Act
        migrate(&mut conn).unwrap();

        // Assert
        assert_eq!(user_version(&conn), crate::SCHEMA_VERSION);
        assert_eq!(
            last_release(&conn).as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn reopening_with_the_same_release_runs_no_step() {
        // Arrange
        let mut conn = Connection::open_in_memory().unwrap();
        let steps = with_extra(&[step(9, "0.5.0", count_table())]);
        migrate_with(&mut conn, &steps, BINARY).unwrap();
        assert_eq!(ran(&conn), 1);

        // Act
        migrate_with(&mut conn, &steps, BINARY).unwrap();

        // Assert
        assert_eq!(ran(&conn), 1);
    }

    #[test]
    fn only_steps_newer_than_the_recorded_release_run() {
        // Arrange
        let mut conn = Connection::open_in_memory().unwrap();
        let old = with_extra(&[step(9, "0.5.0", count_table())]);
        migrate_with(&mut conn, &old, "0.5.0").unwrap();
        let newer = with_extra(&[
            step(9, "0.5.0", count_table()),
            step(10, "0.6.0", count_table()),
        ]);

        // Act
        migrate_with(&mut conn, &newer, "0.6.0").unwrap();

        // Assert
        assert_eq!(ran(&conn), 2);
        assert_eq!(last_release(&conn).as_deref(), Some("0.6.0"));
    }

    #[test]
    fn a_step_list_with_a_release_newer_than_the_binary_is_rejected() {
        // Arrange
        let mut conn = Connection::open_in_memory().unwrap();
        let steps = with_extra(&[step(9, "2.0.0", count_table())]);

        // Act
        let err = migrate_with(&mut conn, &steps, BINARY).unwrap_err();

        // Assert
        assert!(
            matches!(err, StoreError::StepFromFuture { seq: 9, .. }),
            "{err}"
        );
        assert_eq!(user_version(&conn), 0);
    }

    #[test]
    fn a_database_from_a_newer_release_is_refused_and_left_alone() {
        // Arrange
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, STEPS, "3.0.0").unwrap();
        let steps = with_extra(&[step(9, "1.0.0", count_table())]);

        // Act
        let err = migrate_with(&mut conn, &steps, BINARY).unwrap_err();

        // Assert
        assert!(matches!(err, StoreError::ReleaseTooNew { .. }), "{err}");
        assert_eq!(user_version(&conn), crate::SCHEMA_VERSION);
        assert_eq!(last_release(&conn).as_deref(), Some("3.0.0"));
        assert_eq!(ran(&conn), 0);
    }

    #[test]
    fn a_failing_step_rolls_back_and_keeps_the_earlier_ones_and_the_recorded_release() {
        // Arrange
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, STEPS, "0.1.0").unwrap();
        let steps = with_extra(&[
            step(9, "0.5.0", count_table()),
            step(
                10,
                "0.6.0",
                "INSERT INTO ran VALUES (2); INSERT INTO no_such_table VALUES (1);",
            ),
            step(11, "0.7.0", count_table()),
        ]);

        // Act
        let err = migrate_with(&mut conn, &steps, "0.7.0").unwrap_err();

        // Assert
        assert!(matches!(err, StoreError::Sqlite(_)), "{err}");
        assert_eq!(user_version(&conn), 9);
        assert_eq!(ran(&conn), 1);
        assert_eq!(last_release(&conn).as_deref(), Some("0.1.0"));
    }

    #[test]
    fn a_step_added_within_the_recorded_release_still_runs() {
        // Arrange
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, STEPS, BINARY).unwrap();
        let steps = with_extra(&[step(9, BINARY, count_table())]);

        // Act
        migrate_with(&mut conn, &steps, BINARY).unwrap();

        // Assert
        assert_eq!(ran(&conn), 1);
        assert_eq!(user_version(&conn), 9);
        assert_eq!(last_release(&conn).as_deref(), Some(BINARY));
    }

    #[test]
    fn steps_out_of_order_are_rejected_before_any_write() {
        for extra in [
            vec![
                step(9, "0.5.0", count_table()),
                step(10, "0.4.0", count_table()),
            ],
            vec![
                step(10, "0.5.0", count_table()),
                step(9, "0.5.0", count_table()),
            ],
            vec![
                step(9, "0.5.0", count_table()),
                step(9, "0.5.0", count_table()),
            ],
        ] {
            // Arrange
            let mut conn = Connection::open_in_memory().unwrap();
            let steps = with_extra(&extra);

            // Act
            let err = migrate_with(&mut conn, &steps, BINARY).unwrap_err();

            // Assert
            assert!(matches!(err, StoreError::StepsOutOfOrder { .. }), "{err}");
            assert_eq!(user_version(&conn), 0);
        }
    }

    #[test]
    fn build_metadata_does_not_affect_release_comparison() {
        // Arrange
        let mut conn = Connection::open_in_memory().unwrap();
        migrate_with(&mut conn, STEPS, "1.0.0+a").unwrap();
        let steps = with_extra(&[step(9, "1.0.0+z", count_table())]);

        // Act
        migrate_with(&mut conn, &steps, "1.0.0+b").unwrap();

        // Assert
        assert_eq!(last_release(&conn).as_deref(), Some("1.0.0"));
        assert_eq!(ran(&conn), 1);
    }
}
