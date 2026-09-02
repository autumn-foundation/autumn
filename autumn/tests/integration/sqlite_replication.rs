//! End-to-end proof of the continuous-replication loop (#1628).
//!
//! Runs the real [`Replicator`] against a real `SQLite` database file and a real
//! [`FileDestination`], then destroys the database and rebuilds it from the
//! replica alone:
//!
//! ```text
//! seed → tick → more writes → tick → rm app.db* → restore → row equality
//! ```
//!
//! No container, no network, no `--features sqlite` flip — diesel's `SQLite`
//! backend is in the graph under the plain `db` feature, so this whole file runs
//! in the ordinary `cargo test --workspace` lane. The S3 destination is proved
//! separately against `MinIO` in `sqlite_replication_s3.rs`.
//!
//! `Replicator::tick` takes the instant explicitly, so the point-in-time cases
//! below are deterministic: no sleeping, no flaky clock windows.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::replication::destination::{DestinationError, ReplicaDestination};
use autumn_web::replication::{
    FileDestination, ReplicationSettings, ReplicationStatus, Replicator, restore, segment,
};
use chrono::{DateTime, TimeZone as _, Utc};
use diesel::connection::SimpleConnection as _;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection as _, QueryableByName, RunQueryDsl as _, SqliteConnection, sql_query};

// ─── Fixtures ────────────────────────────────────────────────────────────────

#[derive(QueryableByName, Debug, PartialEq, Eq)]
struct Row {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    v: String,
}

/// Open the database the way Autumn's `SQLite` pool does when replication is on:
/// WAL journal mode with auto-checkpointing disabled, so the replicator is the
/// only component that may checkpoint.
fn open_app_db(path: &Path) -> SqliteConnection {
    let mut conn = SqliteConnection::establish(&path.to_string_lossy()).expect("open sqlite");
    conn.batch_execute(
        "PRAGMA journal_mode = WAL; \
         PRAGMA wal_autocheckpoint = 0; \
         PRAGMA synchronous = NORMAL; \
         PRAGMA busy_timeout = 5000;",
    )
    .expect("pool pragmas");
    conn
}

fn create_table(conn: &mut SqliteConnection) {
    conn.batch_execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL);")
        .expect("create table");
}

fn insert(conn: &mut SqliteConnection, values: &[&str]) {
    for value in values {
        sql_query(format!("INSERT INTO t (v) VALUES ('{value}')"))
            .execute(conn)
            .expect("insert");
    }
}

fn read_rows(path: &Path) -> Vec<Row> {
    let mut conn = SqliteConnection::establish(&path.to_string_lossy()).expect("open restored");
    sql_query("SELECT id, v FROM t ORDER BY id")
        .load(&mut conn)
        .expect("select")
}

fn values(rows: &[Row]) -> Vec<String> {
    rows.iter().map(|r| r.v.clone()).collect()
}

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000 + secs, 0)
        .single()
        .expect("timestamp")
}

fn settings(db: &Path) -> ReplicationSettings {
    ReplicationSettings {
        database_path: db.to_path_buf(),
        root: segment::root_prefix(None, "test"),
        sync_interval: Duration::from_secs(1),
        snapshot_interval: Duration::from_secs(3600),
        max_wal_bytes: 16 * 1024 * 1024,
        retention: Duration::from_secs(7 * 24 * 3600),
        verify_interval: None,
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    db: PathBuf,
    replica_root: PathBuf,
    conn: SqliteConnection,
    replicator: Replicator,
    status: Arc<ReplicationStatus>,
    root: String,
}

impl Harness {
    fn new() -> Self {
        Self::with_destination(|dir| {
            Arc::new(FileDestination::new(dir.join("replica")).expect("destination"))
        })
    }

    fn with_destination(make: impl FnOnce(&Path) -> Arc<dyn ReplicaDestination>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("app.db");
        let mut conn = open_app_db(&db);
        create_table(&mut conn);

        let destination = make(dir.path());
        let status = Arc::new(ReplicationStatus::new(destination.describe()));
        let settings = settings(&db);
        let root = settings.root.clone();
        let replicator = Replicator::new(settings, destination, Arc::clone(&status));
        Self {
            db,
            replica_root: dir.path().join("replica"),
            conn,
            replicator,
            status,
            root,
            _dir: dir,
        }
    }

    fn destination(&self) -> Arc<dyn ReplicaDestination> {
        Arc::clone(self.replicator.destination())
    }

    /// Delete the database and every sidecar — the "the disk died" step.
    fn destroy_database(&mut self) {
        let conn = std::mem::replace(
            &mut self.conn,
            SqliteConnection::establish(":memory:").expect("placeholder"),
        );
        drop(conn);
        for path in [
            self.db.clone(),
            PathBuf::from(format!("{}-wal", self.db.display())),
            PathBuf::from(format!("{}-shm", self.db.display())),
        ] {
            let _ = std::fs::remove_file(path);
        }
        assert!(!self.db.exists(), "the database must really be gone");
    }

    fn restore_to(&self, target: Option<DateTime<Utc>>) -> PathBuf {
        let output = self
            .replica_root
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("restored.db");
        let outcome = restore::restore(self.destination().as_ref(), &self.root, target, &output)
            .expect("restore");
        assert_eq!(outcome.output, output);
        output
    }
}

// ─── AC: continuous replication + fresh-box restore ──────────────────────────

#[test]
fn ships_a_generation_and_restores_the_latest_state_on_a_fresh_path() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["alpha", "beta"]);

    let first = h.replicator.tick(at(0)).expect("first tick");
    assert!(first.snapshot_taken, "the first tick opens a generation");
    assert_eq!(first.segments, 1, "committed frames must ship immediately");
    assert!(first.bytes > 0);

    insert(&mut h.conn, &["gamma"]);
    let second = h.replicator.tick(at(5)).expect("second tick");
    assert!(
        !second.snapshot_taken,
        "no new snapshot without a checkpoint"
    );
    assert_eq!(second.segments, 1);
    assert_eq!(second.pending_bytes, 0, "everything committed is offsite");

    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(values(&read_rows(&restored)), ["alpha", "beta", "gamma"]);
}

#[test]
fn an_idle_database_is_still_protected_by_a_base_snapshot() {
    let mut h = Harness::new();
    // No writes at all beyond the table creation, then nothing more.
    h.replicator.tick(at(0)).expect("tick");
    let quiet = h.replicator.tick(at(1)).expect("tick");
    assert_eq!(quiet.segments, 0, "an idle database ships nothing new");

    h.destroy_database();
    let restored = h.restore_to(None);
    assert!(
        read_rows(&restored).is_empty(),
        "the schema restores even with no rows"
    );
}

// ─── AC: point-in-time restore ───────────────────────────────────────────────

#[test]
fn restores_to_a_chosen_point_in_time_within_the_window() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["before-1", "before-2"]);
    h.replicator.tick(at(100)).expect("tick");

    insert(&mut h.conn, &["after-1"]);
    h.replicator.tick(at(200)).expect("tick");

    insert(&mut h.conn, &["after-2"]);
    h.replicator.tick(at(300)).expect("tick");

    h.destroy_database();

    // Exactly at the first segment's ship time: only what was committed by then.
    let early = h.restore_to(Some(at(100)));
    assert_eq!(values(&read_rows(&early)), ["before-1", "before-2"]);

    // Between the second and third: the second segment is included, the third is not.
    let middle = h.restore_to(Some(at(250)));
    assert_eq!(
        values(&read_rows(&middle)),
        ["before-1", "before-2", "after-1"]
    );

    // Latest.
    let latest = h.restore_to(None);
    assert_eq!(
        values(&read_rows(&latest)),
        ["before-1", "before-2", "after-1", "after-2"]
    );
}

#[test]
fn a_point_in_time_before_the_window_is_refused_rather_than_rounded() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["only"]);
    h.replicator.tick(at(1_000)).expect("tick");

    let err = restore::plan(h.destination().as_ref(), &h.root, Some(at(0)))
        .expect_err("a target before the oldest generation must be refused");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("oldest replicated state"),
        "the error must name the window: {rendered}"
    );
    assert!(
        rendered.contains("retention_hours"),
        "the error must say how to widen the window: {rendered}"
    );
}

#[test]
fn a_restore_plan_reports_the_instant_it_actually_lands_on() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["a"]);
    h.replicator.tick(at(100)).expect("tick");
    insert(&mut h.conn, &["b"]);
    h.replicator.tick(at(200)).expect("tick");

    let plan = restore::plan(h.destination().as_ref(), &h.root, Some(at(250))).expect("plan");
    assert_eq!(plan.requested, Some(at(250)));
    assert_eq!(
        plan.effective,
        at(200),
        "the restore lands on the last segment at or before the target"
    );
    assert_eq!(plan.segments.len(), 2);
}

// ─── AC: refuses a replica that fails verification ───────────────────────────

#[test]
fn a_missing_segment_is_refused_instead_of_silently_truncating() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["one"]);
    h.replicator.tick(at(10)).expect("tick");
    insert(&mut h.conn, &["two"]);
    h.replicator.tick(at(20)).expect("tick");
    insert(&mut h.conn, &["three"]);
    h.replicator.tick(at(30)).expect("tick");

    // Delete the MIDDLE segment. SQLite's own recovery would happily stop at the
    // hole and report success; the restore planner must not.
    let destination = h.destination();
    let keys = destination
        .list(&format!("{}/generations/", h.root))
        .expect("list");
    let middle = keys
        .iter()
        .find(|k| k.contains("/segments/0000000001-"))
        .expect("segment 1 exists")
        .clone();
    destination.delete(&middle).expect("delete");

    let err = restore::plan(destination.as_ref(), &h.root, None)
        .expect_err("a hole in the segment sequence must be refused");
    let rendered = format!("{err}");
    assert!(rendered.contains("missing segment 1"), "{rendered}");
    assert!(rendered.contains("hole in it"), "{rendered}");
}

#[test]
fn a_tampered_segment_is_refused_by_its_digest() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["one"]);
    h.replicator.tick(at(10)).expect("tick");

    let destination = h.destination();
    let key = destination
        .list(&format!("{}/generations/", h.root))
        .expect("list")
        .into_iter()
        .find(|k| k.contains("/segments/"))
        .expect("a segment exists");
    let mut payload = destination.get(&key).expect("get");
    let last = payload.len() - 4;
    payload[last] ^= 0xFF;
    destination.put(&key, &payload).expect("put");

    let output = h.replica_root.join("../tampered.db");
    let err = restore::restore(destination.as_ref(), &h.root, None, &output)
        .expect_err("a tampered segment must be refused");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("verification") || rendered.contains("decompress"),
        "{rendered}"
    );
    assert!(
        !output.exists(),
        "a refused restore must not leave a database behind"
    );
}

#[test]
fn a_generation_without_its_commit_marker_is_ignored() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["kept"]);
    h.replicator.tick(at(10)).expect("tick");

    let destination = h.destination();
    let marker = destination
        .list(&format!("{}/generations/", h.root))
        .expect("list")
        .into_iter()
        .find(|k| k.ends_with(segment::SNAPSHOT_META_OBJECT))
        .expect("marker exists");
    destination.delete(&marker).expect("delete");

    let err = restore::plan(destination.as_ref(), &h.root, None)
        .expect_err("an interrupted generation is not restorable");
    assert!(
        format!("{err}").contains("no complete replica generation"),
        "{err}"
    );
}

// ─── The checkpoint interlock (reverse-brainstorm R1/R5) ─────────────────────

/// A destination that can be switched to failing every write, so the "the
/// offsite endpoint went away" case is exercised without a network.
struct FlakyDestination {
    inner: FileDestination,
    failing: Arc<AtomicBool>,
    writes: Arc<Mutex<usize>>,
}

impl FlakyDestination {
    /// The failure a wedged offsite endpoint returns.
    fn fail() -> Result<(), DestinationError> {
        Err(DestinationError::Remote {
            op: "upload",
            status: 503,
            code: Some("SlowDown".to_owned()),
        })
    }
}

impl ReplicaDestination for FlakyDestination {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn put(&self, key: &str, body: &[u8]) -> Result<(), DestinationError> {
        if self.failing.load(Ordering::SeqCst) {
            return Self::fail();
        }
        *self.writes.lock().expect("lock") += 1;
        self.inner.put(key, body)
    }
    fn put_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        if self.failing.load(Ordering::SeqCst) {
            return Self::fail();
        }
        *self.writes.lock().expect("lock") += 1;
        self.inner.put_file(key, path)
    }
    fn get(&self, key: &str) -> Result<Vec<u8>, DestinationError> {
        self.inner.get(key)
    }
    fn get_to_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        self.inner.get_to_file(key, path)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, DestinationError> {
        self.inner.list(prefix)
    }
    fn delete(&self, key: &str) -> Result<(), DestinationError> {
        self.inner.delete(key)
    }
}

#[test]
fn a_dead_destination_stalls_the_checkpoint_rather_than_dropping_data() {
    let failing = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&failing);
    let mut h = Harness::with_destination(move |dir| {
        Arc::new(FlakyDestination {
            inner: FileDestination::new(dir.join("replica")).expect("destination"),
            failing: flag,
            writes: Arc::new(Mutex::new(0)),
        })
    });
    // Force the WAL over the checkpoint budget on every tick.
    insert(&mut h.conn, &["seed"]);
    h.replicator.tick(at(0)).expect("first tick");

    failing.store(true, Ordering::SeqCst);
    insert(&mut h.conn, &["lost-if-broken"]);

    let wal = PathBuf::from(format!("{}-wal", h.db.display()));
    let before = std::fs::metadata(&wal).expect("wal").len();
    let err = h.replicator.tick(at(10)).expect_err("shipping must fail");
    assert!(format!("{err}").contains("503"), "{err}");
    let after = std::fs::metadata(&wal).expect("wal").len();
    assert_eq!(
        before, after,
        "an un-shippable WAL must never be checkpointed away"
    );

    // When the destination comes back, the pending frames ship and the data is
    // still there.
    failing.store(false, Ordering::SeqCst);
    let recovered = h.replicator.tick(at(20)).expect("tick");
    assert_eq!(recovered.segments, 1);
    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(values(&read_rows(&restored)), ["seed", "lost-if-broken"]);
}

#[test]
fn a_checkpoint_rotates_the_generation_and_a_restore_spans_the_rotation() {
    let mut h = Harness::new();
    // A tiny WAL budget makes the checkpoint fire on the very next tick.
    let mut tight = settings(&h.db);
    tight.max_wal_bytes = 1;
    h.replicator = Replicator::new(tight, h.destination(), Arc::clone(&h.status));

    insert(&mut h.conn, &["gen-one"]);
    let first = h.replicator.tick(at(0)).expect("tick");
    assert!(
        first.checkpointed,
        "an over-budget WAL must be checkpointed"
    );

    insert(&mut h.conn, &["gen-two"]);
    let second = h.replicator.tick(at(10)).expect("tick");
    assert!(
        second.snapshot_taken,
        "a checkpointed WAL starts a fresh generation"
    );

    let generations: std::collections::BTreeSet<String> = h
        .destination()
        .list(&format!("{}/generations/", h.root))
        .expect("list")
        .iter()
        .filter_map(|k| segment::generation_of_key(&h.root, k))
        .collect();
    assert!(
        generations.len() >= 2,
        "expected at least two generations, got {generations:?}"
    );

    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(values(&read_rows(&restored)), ["gen-one", "gen-two"]);
}

// ─── AC: safe against a live app ─────────────────────────────────────────────

#[test]
fn replication_never_blocks_or_corrupts_a_live_writer() {
    let mut h = Harness::new();
    let mut tight = settings(&h.db);
    // Checkpoint aggressively: the most contentious thing the replicator does.
    tight.max_wal_bytes = 4096;
    h.replicator = Replicator::new(tight, h.destination(), Arc::clone(&h.status));

    let mut expected = Vec::new();
    for round in 0..40 {
        let value = format!("row-{round}");
        insert(&mut h.conn, &[&value]);
        expected.push(value);
        // Interleave a replication tick with every write, the worst case for
        // contention on the single writer.
        h.replicator
            .tick(at(round))
            .unwrap_or_else(|e| panic!("tick {round} failed: {e}"));
    }

    // The live database is intact…
    assert_eq!(values(&read_rows(&h.db)), expected);
    // …and so is the replica.
    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(values(&read_rows(&restored)), expected);
}

// ─── AC: lag is observable, and verification is a real restore ───────────────

#[test]
fn lag_and_generation_are_observable_to_the_operator() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["a"]);
    h.replicator.tick(at(0)).expect("tick");

    let snapshot = h.status.snapshot();
    assert_eq!(snapshot.last_success_at, Some(at(0)));
    assert_eq!(snapshot.segments_shipped, 1);
    assert_eq!(snapshot.snapshots_taken, 1);
    assert!(snapshot.generation.is_some());
    assert!(snapshot.bytes_shipped > 0);
    assert!(snapshot.destination.starts_with("file://"));
    assert_eq!(snapshot.lag(at(7)), Some(Duration::from_secs(7)));
    assert_eq!(snapshot.pending_bytes, 0);

    // A failing tick must not move the replication point forward.
    h.destroy_database();
    let err = h.replicator.tick(at(50)).expect_err("no database");
    assert!(format!("{err}").contains("does not exist"), "{err}");
    let after = h.status.snapshot();
    assert_eq!(after.last_success_at, Some(at(0)), "lag must keep growing");
    assert_eq!(after.consecutive_failures, 1);
}

#[test]
fn verification_actually_restores_and_fails_loudly_on_a_damaged_replica() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["verified"]);
    h.replicator.tick(at(0)).expect("tick");

    h.replicator.verify().expect("a healthy replica verifies");

    // Corrupt the base snapshot; verification must now fail rather than pass on
    // the strength of the object merely existing.
    let destination = h.destination();
    let snapshot_key = destination
        .list(&format!("{}/generations/", h.root))
        .expect("list")
        .into_iter()
        .find(|k| k.ends_with(segment::SNAPSHOT_OBJECT))
        .expect("snapshot exists");
    destination
        .put(&snapshot_key, b"not a gzip stream at all")
        .expect("put");

    let err = h
        .replicator
        .verify()
        .expect_err("a damaged replica must fail verification");
    assert!(!format!("{err}").is_empty());
}

// ─── Retention ───────────────────────────────────────────────────────────────

#[test]
fn retention_prunes_expired_generations_but_keeps_the_window_floor() {
    let mut h = Harness::new();
    let mut short = settings(&h.db);
    short.max_wal_bytes = 1; // rotate a generation on every tick
    short.retention = Duration::from_secs(60);
    h.replicator = Replicator::new(short, h.destination(), Arc::clone(&h.status));

    for round in 0..5 {
        insert(&mut h.conn, &[&format!("row-{round}")]);
        h.replicator
            .tick(at(round * 100))
            .unwrap_or_else(|e| panic!("tick {round} failed: {e}"));
    }

    let generations: std::collections::BTreeSet<String> = h
        .destination()
        .list(&format!("{}/generations/", h.root))
        .expect("list")
        .iter()
        .filter_map(|k| segment::generation_of_key(&h.root, k))
        .collect();
    assert!(
        generations.len() < 5,
        "expired generations must be pruned, still have {generations:?}"
    );

    // Whatever survived must still restore.
    h.destroy_database();
    let restored = h.restore_to(None);
    assert!(!read_rows(&restored).is_empty());
}
