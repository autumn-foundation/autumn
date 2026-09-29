//! The narrow set of `SQLite` operations continuous replication needs: opening a
//! private connection to a database *file*, checkpointing, and integrity
//! checking (issue #1628).
//!
//! These are deliberately separate from `autumn_web::db`'s runtime pool. The
//! replicator must work on the database file regardless of which backend the
//! build's `RuntimeConnection` alias points at, and it must not borrow a
//! connection from the app's pool — a checkpoint on a pooled connection would
//! contend with request traffic. `diesel`'s `SQLite` backend is in the graph under
//! the plain `db` feature, so this needs no `--features sqlite` flip.

// autumn-panic-gate: durability-critical module — production code path must be
// panic-free. See CONTRIBUTING.md "Request-path panic gate". Justify exceptions
// with #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::fmt;
use std::path::Path;

use diesel::connection::SimpleConnection as _;
use diesel::prelude::*;
use diesel::sql_types::{Integer, Text};

use super::wal;

/// How long a replicator connection waits for a lock before giving up.
///
/// Deliberately short. A checkpoint that cannot get in *right now* is retried on
/// the next tick; blocking here would be the replicator holding up the app's
/// single writer, which #1628's AC #3 forbids.
const BUSY_TIMEOUT_MS: u32 = 1_000;

/// A `SQLite` operation the replicator performs failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SqliteError {
    /// The database file could not be opened.
    Open {
        /// Path that was opened (never a URL with credentials — `SQLite` has none).
        path: String,
        /// diesel/`SQLite` detail.
        detail: String,
    },
    /// A pragma or query failed.
    Query {
        /// What was being run.
        op: &'static str,
        /// diesel/`SQLite` detail.
        detail: String,
    },
    /// `PRAGMA integrity_check` reported something other than `ok`.
    IntegrityFailed {
        /// The first line `SQLite` reported.
        detail: String,
    },
}

impl fmt::Display for SqliteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, detail } => {
                write!(f, "could not open the SQLite database at {path}: {detail}")
            }
            Self::Query { op, detail } => write!(f, "SQLite {op} failed: {detail}"),
            Self::IntegrityFailed { detail } => {
                write!(f, "SQLite integrity check failed: {detail}")
            }
        }
    }
}

impl std::error::Error for SqliteError {}

/// Row shape for `PRAGMA integrity_check`.
#[derive(QueryableByName)]
struct IntegrityRow {
    #[diesel(sql_type = Text)]
    integrity_check: String,
}

/// Row shape for `PRAGMA wal_checkpoint(...)`: `busy`, `log`, `checkpointed`.
#[derive(QueryableByName)]
struct CheckpointRow {
    #[diesel(sql_type = Integer)]
    busy: i32,
}

/// Row shape for `PRAGMA data_version`.
#[derive(QueryableByName)]
struct DataVersionRow {
    #[diesel(sql_type = Integer)]
    data_version: i32,
}

/// Open a private connection to `path` with a short busy timeout and
/// auto-checkpointing disabled.
///
/// Auto-checkpointing is off because the replicator is the **only** component
/// allowed to checkpoint: an auto-checkpoint would restart the WAL and overwrite
/// frames that have not been shipped yet (reverse-brainstorm R1).
///
/// # Errors
///
/// Returns [`SqliteError::Open`] or [`SqliteError::Query`].
pub fn open(path: &Path) -> Result<SqliteConnection, SqliteError> {
    // Through `connection_string`, not the raw path: diesel opens with
    // `SQLITE_OPEN_URI`, so a filename beginning with `file:` would be re-read as
    // a URI and name a different database.
    let Some(target) = super::connection_string(path) else {
        return Err(SqliteError::Open {
            path: path.display().to_string(),
            detail: "the path is not valid UTF-8".to_owned(),
        });
    };
    let mut conn = SqliteConnection::establish(&target).map_err(|e| SqliteError::Open {
        path: target.clone(),
        detail: e.to_string(),
    })?;
    conn.batch_execute(&format!(
        "PRAGMA busy_timeout = {BUSY_TIMEOUT_MS}; PRAGMA wal_autocheckpoint = 0;"
    ))
    .map_err(|e| SqliteError::Query {
        op: "connection pragmas",
        detail: e.to_string(),
    })?;
    Ok(conn)
}

/// What a checkpoint attempt achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointOutcome {
    /// The WAL is now empty: every frame was written back to the main database
    /// file and the `-wal` was truncated to zero bytes.
    Truncated,
    /// Another connection held a read or write lock, so the WAL still holds
    /// frames. Harmless — the next tick tries again.
    Busy {
        /// Bytes still in the `-wal` file.
        wal_bytes: u64,
    },
}

/// Run `PRAGMA wal_checkpoint(TRUNCATE)` and report what it achieved.
///
/// `SQLite` reports a blocked checkpoint as a *result row* rather than an error,
/// so the pragma's own `busy` column is the authority — not the size of the
/// `-wal` file afterwards. That distinction matters: on a busy database an app
/// writer routinely starts the *next* WAL microseconds after a successful
/// checkpoint, so a file-size check reports a perfectly good checkpoint as
/// blocked, and the replicator would then re-upload a whole base snapshot it
/// did not need.
///
/// # Errors
///
/// Returns [`SqliteError::Query`] when the pragma itself fails.
pub fn checkpoint_truncate(
    conn: &mut SqliteConnection,
    db_path: &Path,
) -> Result<CheckpointOutcome, SqliteError> {
    let rows: Vec<CheckpointRow> = diesel::sql_query("PRAGMA wal_checkpoint(TRUNCATE)")
        .load(conn)
        .map_err(|e| SqliteError::Query {
            op: "wal_checkpoint(TRUNCATE)",
            detail: e.to_string(),
        })?;
    let blocked = rows.first().is_none_or(|row| row.busy != 0);
    if blocked {
        let wal_bytes = std::fs::metadata(wal::wal_path(db_path)).map_or(0, |m| m.len());
        return Ok(CheckpointOutcome::Busy { wal_bytes });
    }
    Ok(CheckpointOutcome::Truncated)
}

/// Read `PRAGMA data_version`, which changes whenever **another connection**
/// commits (and never for this connection's own writes).
///
/// The replicator brackets its checkpoint with two reads of this counter: if it
/// did not move, no other connection committed while the checkpoint was taken,
/// which is exactly the proof that the checkpoint folded away nothing the
/// replicator had not already shipped.
///
/// # Errors
///
/// Returns [`SqliteError::Query`] when the pragma fails.
pub fn data_version(conn: &mut SqliteConnection) -> Result<i32, SqliteError> {
    let rows: Vec<DataVersionRow> = diesel::sql_query("PRAGMA data_version")
        .load(conn)
        .map_err(|e| SqliteError::Query {
            op: "data_version",
            detail: e.to_string(),
        })?;
    rows.first()
        .map(|row| row.data_version)
        .ok_or_else(|| SqliteError::Query {
            op: "data_version",
            detail: "the pragma returned no rows".to_owned(),
        })
}

/// Run `PRAGMA integrity_check` and fail unless `SQLite` answers exactly `ok`.
///
/// # Errors
///
/// Returns [`SqliteError::Query`] if the pragma cannot run, or
/// [`SqliteError::IntegrityFailed`] with `SQLite`'s first complaint.
pub fn integrity_check(conn: &mut SqliteConnection) -> Result<(), SqliteError> {
    let rows: Vec<IntegrityRow> = diesel::sql_query("PRAGMA integrity_check")
        .load(conn)
        .map_err(|e| SqliteError::Query {
            op: "integrity_check",
            detail: e.to_string(),
        })?;
    match rows.first() {
        Some(row) if row.integrity_check == "ok" && rows.len() == 1 => Ok(()),
        Some(row) => Err(SqliteError::IntegrityFailed {
            detail: row.integrity_check.clone(),
        }),
        None => Err(SqliteError::IntegrityFailed {
            detail: "integrity_check returned no rows".to_owned(),
        }),
    }
}

/// Put `path` into WAL journal mode, returning the mode `SQLite` reports.
///
/// # Errors
///
/// Returns [`SqliteError::Query`] when the pragma fails.
pub fn ensure_wal_mode(conn: &mut SqliteConnection) -> Result<(), SqliteError> {
    conn.batch_execute("PRAGMA journal_mode = WAL;")
        .map_err(|e| SqliteError::Query {
            op: "journal_mode = WAL",
            detail: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(dir: &Path) -> (std::path::PathBuf, SqliteConnection) {
        let db = dir.join("app.db");
        let mut conn = open(&db).expect("open");
        ensure_wal_mode(&mut conn).expect("wal");
        conn.batch_execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL); \
             INSERT INTO t (v) VALUES ('a'), ('b');",
        )
        .expect("seed");
        (db, conn)
    }

    #[test]
    fn checkpoint_truncate_empties_the_wal_and_integrity_passes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (db, mut conn) = seeded(dir.path());
        assert!(
            std::fs::metadata(wal::wal_path(&db))
                .expect("wal exists")
                .len()
                > 0,
            "the seed must have left frames in the WAL"
        );
        assert_eq!(
            checkpoint_truncate(&mut conn, &db).expect("checkpoint"),
            CheckpointOutcome::Truncated
        );
        integrity_check(&mut conn).expect("integrity");
    }

    #[test]
    fn a_second_open_connection_does_not_break_the_checkpoint_report() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (db, mut conn) = seeded(dir.path());
        let _reader = open(&db).expect("second connection");
        // Whatever the outcome, it must be reported honestly rather than
        // claiming success while frames remain.
        let outcome = checkpoint_truncate(&mut conn, &db).expect("checkpoint");
        let wal_bytes = std::fs::metadata(wal::wal_path(&db)).map_or(0, |m| m.len());
        match outcome {
            CheckpointOutcome::Truncated => assert_eq!(wal_bytes, 0),
            CheckpointOutcome::Busy { wal_bytes: n } => assert_eq!(n, wal_bytes),
        }
    }

    #[test]
    fn integrity_check_reports_a_corrupted_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("broken.db");
        {
            let (path, mut conn) = seeded(dir.path());
            checkpoint_truncate(&mut conn, &path).expect("checkpoint");
            drop(conn);
            std::fs::copy(&path, &db).expect("copy");
        }
        // Corrupt the middle of the file (past the header so it still opens).
        let mut bytes = std::fs::read(&db).expect("read");
        let len = bytes.len();
        for i in (len / 2)..(len / 2 + 256).min(len) {
            if let Some(b) = bytes.get_mut(i) {
                *b ^= 0xFF;
            }
        }
        std::fs::write(&db, &bytes).expect("write");

        let mut conn = open(&db).expect("open");
        assert!(
            integrity_check(&mut conn).is_err(),
            "a corrupted database must fail the integrity check"
        );
    }
}
