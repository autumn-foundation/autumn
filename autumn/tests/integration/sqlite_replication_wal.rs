//! Phase 0 (#1628): the `SQLite` write-ahead-log reader the replicator ships from.
//!
//! These tests run against a **real** `SQLite` database file (diesel's `SQLite`
//! backend is in the graph under the plain `db` feature, so no `--features
//! sqlite` flip and no container is needed) and assert the three properties the
//! whole replication story rests on:
//!
//! * the WAL header parses, and its salt identifies the *generation*;
//! * a scan validates `SQLite`'s frame checksum chain and reports the last
//!   **commit** boundary, so a half-written tail frame is never shipped (R2);
//! * a `wal_checkpoint(TRUNCATE)` starts a new salt sequence (R3).

use std::path::Path;

use autumn_web::replication::wal::{self, ScanCursor, WalHeader};
use diesel::connection::SimpleConnection as _;
use diesel::{Connection as _, RunQueryDsl as _, SqliteConnection, sql_query};

/// Narrow a WAL byte offset for slicing. These WAL files are kilobytes, so the
/// conversion always succeeds; a failure would be a bug in the fixture.
fn offset(value: u64) -> usize {
    usize::try_from(value).expect("a WAL offset fits in usize")
}

/// Open a WAL-mode `SQLite` connection with auto-checkpointing disabled — the
/// exact posture the replicator installs (R1).
fn open_wal_db(path: &Path) -> SqliteConnection {
    let mut conn = SqliteConnection::establish(&path.to_string_lossy()).expect("open sqlite");
    conn.batch_execute(
        "PRAGMA journal_mode = WAL; \
         PRAGMA wal_autocheckpoint = 0; \
         PRAGMA synchronous = NORMAL;",
    )
    .expect("wal pragmas");
    conn
}

fn seed(conn: &mut SqliteConnection, rows: usize) {
    conn.batch_execute("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT NOT NULL);")
        .expect("create table");
    for i in 0..rows {
        sql_query(format!("INSERT INTO t (v) VALUES ('row-{i}')"))
            .execute(conn)
            .expect("insert");
    }
}

#[test]
fn wal_header_parses_and_carries_the_generation_salt() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("app.db");
    let mut conn = open_wal_db(&db);
    seed(&mut conn, 5);

    let bytes = std::fs::read(db.with_extension("db-wal")).expect("read -wal");
    let header = WalHeader::parse(&bytes).expect("parse wal header");

    assert!(
        header.page_size >= 512 && header.page_size.is_power_of_two(),
        "page size must be a real SQLite page size, got {}",
        header.page_size
    );
    assert_eq!(header.salt(), (header.salt1, header.salt2));
    // The header's own checksum must validate (bytes 0..24, seeded 0/0).
    assert!(
        WalHeader::parse(&bytes).is_ok(),
        "a real SQLite WAL header must validate"
    );
}

#[test]
fn scan_reports_the_last_commit_boundary_and_ignores_a_torn_tail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("app.db");
    let mut conn = open_wal_db(&db);
    seed(&mut conn, 3);

    let wal_path = db.with_extension("db-wal");
    let bytes = std::fs::read(&wal_path).expect("read -wal");
    let header = WalHeader::parse(&bytes).expect("header");
    let cursor = ScanCursor::start(&header);
    let scan = wal::scan_from(&header, &cursor, &bytes[offset(cursor.offset)..]).expect("scan");

    assert!(scan.commits >= 1, "at least one commit must be visible");
    assert_eq!(
        offset(scan.last_commit_end),
        bytes.len(),
        "a quiesced WAL ends exactly on a commit boundary"
    );
    assert!(scan.last_commit_db_pages > 0);

    // Append 12 bytes of garbage: a torn frame the writer never finished.
    let mut torn = bytes;
    torn.extend_from_slice(&[0xAB; 12]);
    let torn_scan = wal::scan_from(&header, &cursor, &torn[offset(cursor.offset)..]).expect("scan");
    assert_eq!(
        torn_scan.last_commit_end, scan.last_commit_end,
        "a torn tail must never advance the shippable commit boundary"
    );
}

#[test]
fn scan_stops_at_a_broken_checksum_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("app.db");
    let mut conn = open_wal_db(&db);
    seed(&mut conn, 40);

    let wal_path = db.with_extension("db-wal");
    let bytes = std::fs::read(&wal_path).expect("read -wal");
    let header = WalHeader::parse(&bytes).expect("header");
    let cursor = ScanCursor::start(&header);
    let clean = wal::scan_from(&header, &cursor, &bytes[offset(cursor.offset)..]).expect("scan");
    assert!(clean.frames >= 2, "need several frames for this test");

    // Corrupt a byte inside the FIRST frame's page payload.
    let mut broken = bytes;
    let first_page_byte = wal::WAL_HEADER_SIZE + wal::FRAME_HEADER_SIZE + 4;
    broken[first_page_byte] ^= 0xFF;
    let broken_scan =
        wal::scan_from(&header, &cursor, &broken[offset(cursor.offset)..]).expect("scan");
    assert_eq!(
        broken_scan.frames, 0,
        "a broken checksum chain must yield no valid frames from that point on"
    );
    assert_eq!(broken_scan.last_commit_end, cursor.offset);
}

#[test]
fn truncate_checkpoint_starts_a_new_salt_generation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("app.db");
    let mut conn = open_wal_db(&db);
    seed(&mut conn, 5);

    let wal_path = db.with_extension("db-wal");
    let before = WalHeader::parse(&std::fs::read(&wal_path).expect("read")).expect("header");

    conn.batch_execute("PRAGMA wal_checkpoint(TRUNCATE);")
        .expect("checkpoint");
    assert_eq!(
        std::fs::metadata(&wal_path).expect("stat").len(),
        0,
        "TRUNCATE checkpoint empties the -wal file"
    );

    seed(&mut conn, 5);
    let after = WalHeader::parse(&std::fs::read(&wal_path).expect("read")).expect("header");
    assert_ne!(
        before.salt(),
        after.salt(),
        "a checkpoint must start a new generation salt"
    );
}
