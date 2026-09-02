//! Fresh-box recovery through the real `autumn` binary (#1628, phase 2).
//!
//! The acceptance criterion is specific: *from a fresh machine with only the
//! destination credentials, one command restores the latest replicated state,
//! and an optional timestamp flag restores to a chosen point in time.* So this
//! test does exactly that, as a subprocess:
//!
//! ```text
//! machine A: seed a SQLite database, replicate it to a destination
//! machine B: a directory containing ONLY autumn.toml
//!            → autumn db replica status
//!            → autumn db replica restore
//!            → autumn db replica restore --timestamp … --force
//! ```
//!
//! "Machine A" writes the replica through `autumn_web`'s replicator (the same
//! code the running app uses) with explicit tick timestamps, so the point-in-time
//! assertions are deterministic. "Machine B" only ever sees the `autumn` binary,
//! an `autumn.toml`, and the destination — never machine A's files.
//!
//! The destination is a directory rather than S3 so the whole thing runs in the
//! ordinary test lane; `offsite_backup.rs` covers the CLI's S3 transport against
//! `MinIO`, and `autumn-web`'s `sqlite_replication_s3` covers replication's.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::replication::{
    FileDestination, ReplicaDestination as _, ReplicationSettings, ReplicationStatus, Replicator,
    segment,
};
use chrono::{DateTime, TimeZone as _, Utc};
use diesel::connection::SimpleConnection as _;
use diesel::sql_types::Text;
use diesel::{Connection as _, QueryableByName, RunQueryDsl as _, SqliteConnection, sql_query};

const fn autumn_bin() -> &'static str {
    env!("CARGO_BIN_EXE_autumn")
}

#[derive(QueryableByName)]
struct Value {
    #[diesel(sql_type = Text)]
    v: String,
}

fn run_autumn(dir: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let output = Command::new(autumn_bin())
        .args(args)
        .current_dir(dir)
        // A fresh box: the profile is the only ambient input, and the
        // destination needs no credentials because it is a directory.
        .env("AUTUMN_ENV", "dev")
        .env("AUTUMN_MANIFEST_DIR", dir)
        .output()
        .expect("failed to run autumn");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code(),
    )
}

fn run_autumn_ok(dir: &Path, args: &[&str]) -> (String, String) {
    let (stdout, stderr, code) = run_autumn(dir, args);
    assert_eq!(
        code,
        Some(0),
        "autumn {args:?} failed (exit={code:?})\nstdout: {stdout}\nstderr: {stderr}"
    );
    (stdout, stderr)
}

/// A fixture instant, deliberately in the **past** (2023-11-14 + `secs`).
///
/// Anything that compares a replication timestamp against `Utc::now()` — lag,
/// retention — must see these as history, so the suite's meaning cannot change
/// on a calendar date.
fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 0)
        .single()
        .expect("timestamp")
}

fn read_values(path: &Path) -> Vec<String> {
    let mut conn = SqliteConnection::establish(&path.to_string_lossy()).expect("open restored");
    sql_query("SELECT v FROM t ORDER BY id")
        .load::<Value>(&mut conn)
        .expect("select")
        .into_iter()
        .map(|row| row.v)
        .collect()
}

/// Seed a database on "machine A" and replicate it, returning the replica
/// directory and the two instants the two batches were shipped at.
fn replicate_two_batches(root_dir: &Path) -> PathBuf {
    let source_dir = root_dir.join("machine-a");
    std::fs::create_dir_all(&source_dir).expect("machine-a");
    let db = source_dir.join("app.db");

    let mut conn = SqliteConnection::establish(&db.to_string_lossy()).expect("open sqlite");
    conn.batch_execute(
        "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0; \
         CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL); \
         INSERT INTO t (v) VALUES ('before-1'), ('before-2');",
    )
    .expect("seed");

    let replica = root_dir.join("replica");
    let destination = Arc::new(FileDestination::new(&replica).expect("destination"));
    let status = Arc::new(ReplicationStatus::new(destination.describe()));
    let mut replicator = Replicator::new(
        ReplicationSettings {
            database_path: db,
            root: segment::root_prefix(Some("db"), "dev"),
            sync_interval: Duration::from_secs(1),
            snapshot_interval: Duration::from_secs(3600),
            max_wal_bytes: 16 * 1024 * 1024,
            retention: Duration::from_secs(7 * 24 * 3600),
            verify_interval: None,
        },
        destination,
        status,
    );

    replicator.tick(at(100)).expect("first tick");
    sql_query("INSERT INTO t (v) VALUES ('after-1')")
        .execute(&mut conn)
        .expect("insert");
    replicator.tick(at(200)).expect("second tick");

    // Machine A is now irrelevant: delete it outright so nothing below can
    // accidentally read from it.
    drop(conn);
    std::fs::remove_dir_all(&source_dir).expect("destroy machine A");
    replica
}

/// Write the only file "machine B" has: an `autumn.toml` naming the destination
/// and where the database belongs.
fn write_machine_b(dir: &Path, replica: &Path, database: &Path) {
    std::fs::create_dir_all(dir).expect("machine-b");
    std::fs::write(
        dir.join("autumn.toml"),
        format!(
            "[database]\nurl = \"sqlite://{database}\"\n\n\
             [replication]\nenabled = true\nprefix = \"db\"\npath = \"{replica}\"\n",
            database = database.display(),
            replica = replica.display(),
        ),
    )
    .expect("write autumn.toml");
}

#[test]
fn a_fresh_box_restores_the_latest_state_with_one_command() {
    let dir = tempfile::tempdir().expect("tempdir");
    let replica = replicate_two_batches(dir.path());
    let machine_b = dir.path().join("machine-b");
    let database = machine_b.join("app.db");
    write_machine_b(&machine_b, &replica, &database);

    // Nothing here but the config file.
    assert!(!database.exists());

    // `--json` rather than the padded table: this is the monitoring surface, and
    // an assertion on column alignment would be a false failure waiting to happen.
    let (stdout, _) = run_autumn_ok(&machine_b, &["db", "replica", "status", "--json"]);
    let report: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("status JSON: {e}\n{stdout}"));
    // One segment: the tick that opens a generation checkpoints first, so the
    // first batch rides in the base snapshot rather than in a WAL segment.
    assert_eq!(report["segments"], 1, "{report}");
    assert!(report["generation"].is_string(), "{report}");
    assert!(report["replication_lag_seconds"].is_number(), "{report}");
    assert_eq!(report["rpo_seconds"], 10, "{report}");

    let (_, stderr) = run_autumn_ok(&machine_b, &["db", "replica", "restore"]);
    assert!(
        stderr.contains("integrity verified"),
        "restore output: {stderr}"
    );
    assert!(stderr.contains("Restored"), "restore output: {stderr}");
    assert_eq!(read_values(&database), ["before-1", "before-2", "after-1"]);
}

#[test]
fn a_timestamp_flag_restores_to_a_chosen_point_in_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let replica = replicate_two_batches(dir.path());
    let machine_b = dir.path().join("machine-b");
    let database = machine_b.join("app.db");
    write_machine_b(&machine_b, &replica, &database);

    let target = at(150).to_rfc3339();
    let (_, stderr) = run_autumn_ok(
        &machine_b,
        &["db", "replica", "restore", "--timestamp", &target],
    );
    assert!(stderr.contains("requested"), "restore output: {stderr}");
    assert_eq!(
        read_values(&database),
        ["before-1", "before-2"],
        "a point-in-time restore must exclude what was committed later"
    );
}

#[test]
fn overwriting_an_existing_database_needs_force() {
    let dir = tempfile::tempdir().expect("tempdir");
    let replica = replicate_two_batches(dir.path());
    let machine_b = dir.path().join("machine-b");
    let database = machine_b.join("app.db");
    write_machine_b(&machine_b, &replica, &database);

    run_autumn_ok(&machine_b, &["db", "replica", "restore"]);

    let (_, stderr, code) = run_autumn(&machine_b, &["db", "replica", "restore"]);
    assert_ne!(code, Some(0), "a second restore must refuse: {stderr}");
    assert!(stderr.contains("already exists"), "{stderr}");
    assert!(stderr.contains("--overwrite"), "{stderr}");

    // `--force` alone is about the PROFILE, not about data: a recovery drill
    // that always passes it must not silently destroy a live database.
    let (_, stderr, code) = run_autumn(&machine_b, &["db", "replica", "restore", "--force"]);
    assert_ne!(
        code,
        Some(0),
        "--force must not imply --overwrite: {stderr}"
    );
    assert!(stderr.contains("already exists"), "{stderr}");

    // With --overwrite it goes through, and the data is the same.
    run_autumn_ok(&machine_b, &["db", "replica", "restore", "--overwrite"]);
    assert_eq!(read_values(&database), ["before-1", "before-2", "after-1"]);
}

#[test]
fn a_target_outside_the_retention_window_is_refused_not_rounded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let replica = replicate_two_batches(dir.path());
    let machine_b = dir.path().join("machine-b");
    let database = machine_b.join("app.db");
    write_machine_b(&machine_b, &replica, &database);

    let (_, stderr, code) = run_autumn(
        &machine_b,
        &[
            "db",
            "replica",
            "restore",
            "--timestamp",
            "2000-01-01T00:00:00Z",
        ],
    );
    assert_ne!(code, Some(0), "{stderr}");
    assert!(stderr.contains("oldest replicated state"), "{stderr}");
    assert!(
        !database.exists(),
        "a refused restore must not leave a database behind"
    );
}

#[test]
fn a_malformed_timestamp_is_rejected_with_an_example() {
    let dir = tempfile::tempdir().expect("tempdir");
    let replica = replicate_two_batches(dir.path());
    let machine_b = dir.path().join("machine-b");
    let database = machine_b.join("app.db");
    write_machine_b(&machine_b, &replica, &database);

    let (_, stderr, code) = run_autumn(
        &machine_b,
        &["db", "replica", "restore", "--timestamp", "yesterday"],
    );
    assert_ne!(code, Some(0));
    assert!(stderr.contains("RFC 3339"), "{stderr}");
    assert!(stderr.contains("--timestamp 2026-"), "{stderr}");
}

#[test]
fn verify_proves_the_replica_restorable_without_touching_the_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let replica = replicate_two_batches(dir.path());
    let machine_b = dir.path().join("machine-b");
    let database = machine_b.join("app.db");
    write_machine_b(&machine_b, &replica, &database);

    let (_, stderr) = run_autumn_ok(&machine_b, &["db", "replica", "verify"]);
    assert!(stderr.contains("restorable"), "{stderr}");
    assert!(
        !database.exists(),
        "verify must not write the app's database"
    );
}

#[test]
fn a_production_profile_restore_is_refused_without_force() {
    let dir = tempfile::tempdir().expect("tempdir");
    let replica = replicate_two_batches(dir.path());
    let machine_b = dir.path().join("machine-b");
    let database = machine_b.join("app.db");
    write_machine_b(&machine_b, &replica, &database);

    let output = Command::new(autumn_bin())
        .args(["db", "replica", "restore", "--profile", "prod"])
        .current_dir(&machine_b)
        .env("AUTUMN_ENV", "prod")
        .env("AUTUMN_MANIFEST_DIR", &machine_b)
        .output()
        .expect("run autumn");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(output.status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("Refusing to restore"), "{stderr}");
    assert!(stderr.contains("--force"), "{stderr}");
    assert!(!database.exists());
}

#[test]
fn an_unconfigured_destination_says_how_to_configure_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let machine_b = dir.path().join("machine-b");
    std::fs::create_dir_all(&machine_b).expect("machine-b");
    std::fs::write(
        machine_b.join("autumn.toml"),
        "[database]\nurl = \"sqlite://app.db\"\n",
    )
    .expect("write autumn.toml");

    let (_, stderr, code) = run_autumn(&machine_b, &["db", "replica", "status"]);
    assert_ne!(code, Some(0));
    assert!(stderr.contains("[replication]"), "{stderr}");
    assert!(stderr.contains("AUTUMN_REPLICATION__"), "{stderr}");
}
