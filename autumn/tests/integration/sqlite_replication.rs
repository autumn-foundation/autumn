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
//! The replicator reads time from an injected clock, and this suite steps that
//! clock by hand, so the point-in-time cases below are deterministic: no
//! sleeping, no flaky clock windows.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::actuator::{HealthIndicatorRegistry, HealthStatus, IndicatorGroup};
use autumn_web::replication::destination::{DestinationError, ReplicaDestination};
use autumn_web::replication::{
    FileDestination, HealthThresholds, ReplicationHealthIndicator, ReplicationSettings,
    ReplicationStatus, Replicator, TickReport, restore, segment,
};
use autumn_web::time::ClockSource;
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

/// A clock the suite steps by hand.
///
/// The replicator stamps each artifact from its clock at the moment that
/// artifact's contents are fenced — after the checkpoint for a snapshot, after
/// the WAL read for a segment — so the timestamps a point-in-time restore
/// selects on cannot be passed in from the outside. Injecting a clock the test
/// owns keeps that ordering honest *and* the fixture times deterministic.
struct StepClock(Mutex<DateTime<Utc>>);

impl StepClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(at(0))))
    }

    fn set(&self, now: DateTime<Utc>) {
        *self.0.lock().expect("clock") = now;
    }
}

impl ClockSource for StepClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().expect("clock")
    }
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
    clock: Arc<StepClock>,
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
        let clock = StepClock::new();
        let replicator = Replicator::new(settings, destination, Arc::clone(&status))
            .with_clock(Arc::clone(&clock) as Arc<dyn ClockSource>);
        Self {
            db,
            replica_root: dir.path().join("replica"),
            conn,
            replicator,
            status,
            root,
            clock,
            _dir: dir,
        }
    }

    fn destination(&self) -> Arc<dyn ReplicaDestination> {
        Arc::clone(self.replicator.destination())
    }

    /// Move the clock to `at(secs)` and run one tick.
    fn tick(&mut self, secs: i64) -> Result<TickReport, autumn_web::replication::ReplicationError> {
        self.clock.set(at(secs));
        self.replicator.tick()
    }

    /// Swap in a replicator with different settings, keeping this harness's
    /// destination, status and clock.
    fn rebuild(&mut self, settings: ReplicationSettings) {
        self.replicator = Replicator::new(settings, self.destination(), Arc::clone(&self.status))
            .with_clock(Arc::clone(&self.clock) as Arc<dyn ClockSource>);
    }

    /// Remove one object from the replica, the way a lifecycle rule or a stray
    /// delete would.
    fn delete_object(&self, key: &str) {
        self.destination()
            .delete(key)
            .expect("delete replica object");
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
    prime(&mut h, 0);

    insert(&mut h.conn, &["gamma"]);
    let second = h.tick(5).expect("second tick");
    assert!(
        !second.snapshot_taken,
        "no new snapshot without a checkpoint"
    );
    assert_eq!(second.segments, 1, "committed frames must ship immediately");
    assert!(second.bytes > 0);
    assert_eq!(second.pending_bytes, 0, "everything committed is offsite");

    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(values(&read_rows(&restored)), ["alpha", "beta", "gamma"]);
}

#[test]
fn an_idle_database_is_still_protected_by_a_base_snapshot() {
    let mut h = Harness::new();
    // No writes at all beyond the table creation, then nothing more.
    h.tick(0).expect("tick");
    let quiet = h.tick(1).expect("tick");
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
    h.tick(100).expect("tick");

    insert(&mut h.conn, &["after-1"]);
    h.tick(200).expect("tick");

    insert(&mut h.conn, &["after-2"]);
    h.tick(300).expect("tick");

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
    h.tick(1_000).expect("tick");

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
    prime(&mut h, 100);
    insert(&mut h.conn, &["b"]);
    h.tick(200).expect("tick");
    insert(&mut h.conn, &["c"]);
    h.tick(300).expect("tick");

    let plan = restore::plan(h.destination().as_ref(), &h.root, Some(at(250))).expect("plan");
    assert_eq!(plan.requested, Some(at(250)));
    assert_eq!(
        plan.effective,
        at(200),
        "the restore lands on the last segment at or before the target"
    );
    assert_eq!(plan.segments.len(), 1, "the later segment is excluded");
}

// ─── AC: refuses a replica that fails verification ───────────────────────────

#[test]
fn a_missing_segment_is_refused_instead_of_silently_truncating() {
    let mut h = Harness::new();
    prime(&mut h, 0);
    insert(&mut h.conn, &["one"]);
    h.tick(10).expect("tick");
    insert(&mut h.conn, &["two"]);
    h.tick(20).expect("tick");
    insert(&mut h.conn, &["three"]);
    h.tick(30).expect("tick");

    // Delete the MIDDLE segment. SQLite's own recovery would happily stop at the
    // hole and report success; the restore planner must not.
    let destination = h.destination();
    let keys = destination
        .list(&format!("{}/generations/", h.root))
        .expect("list");
    let middle = keys
        .iter()
        .find(|k| k.contains("/segments/00000-0000000001-"))
        .expect("segment 1 exists")
        .clone();
    destination.delete(&middle).expect("delete");

    let err = restore::plan(destination.as_ref(), &h.root, None)
        .expect_err("a hole in the segment sequence must be refused");
    let rendered = format!("{err}");
    assert!(rendered.contains("missing segment 0/1"), "{rendered}");
    assert!(rendered.contains("hole in it"), "{rendered}");
}

#[test]
fn a_tampered_segment_is_refused_by_its_digest() {
    let mut h = Harness::new();
    prime(&mut h, 0);
    insert(&mut h.conn, &["one"]);
    h.tick(10).expect("tick");

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
    h.tick(10).expect("tick");

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
    h.tick(0).expect("first tick");

    failing.store(true, Ordering::SeqCst);
    insert(&mut h.conn, &["lost-if-broken"]);

    let wal = PathBuf::from(format!("{}-wal", h.db.display()));
    let before = std::fs::metadata(&wal).expect("wal").len();
    let err = h.tick(10).expect_err("shipping must fail");
    assert!(format!("{err}").contains("503"), "{err}");
    let after = std::fs::metadata(&wal).expect("wal").len();
    assert_eq!(
        before, after,
        "an un-shippable WAL must never be checkpointed away"
    );

    // When the destination comes back, the pending frames ship and the data is
    // still there.
    failing.store(false, Ordering::SeqCst);
    let recovered = h.tick(20).expect("tick");
    assert_eq!(recovered.segments, 1);
    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(values(&read_rows(&restored)), ["seed", "lost-if-broken"]);
}

/// Run the tick that opens the first generation.
///
/// That tick checkpoints *before* it snapshots, so the base snapshot is never
/// behind the WAL — which also means it ships no segment: everything written so
/// far is already inside the database file it just copied.
fn prime(h: &mut Harness, secs: i64) {
    let report = h.tick(secs).expect("priming tick");
    assert!(report.snapshot_taken, "the first tick opens a generation");
    assert_eq!(
        report.segments, 0,
        "a generation opens from a freshly checkpointed WAL, so it has nothing to ship"
    );
}

/// Tick until the replicator actually checkpoints.
///
/// A checkpoint can be reported `Busy` for reasons outside this test's control
/// (another connection momentarily holding a lock), and production treats that
/// as normal and retries — so the test retries too rather than asserting a
/// checkpoint on the first attempt.
fn tick_until_checkpointed(h: &mut Harness, base: i64) -> TickReport {
    for attempt in 0..20 {
        let report = h
            .tick(base + attempt)
            .unwrap_or_else(|e| panic!("tick failed: {e}"));
        if report.checkpointed {
            return report;
        }
    }
    panic!("the replicator never checkpointed an over-budget WAL");
}

/// Every segment key on the destination, in replication order.
fn segment_keys_on(h: &Harness) -> Vec<String> {
    let mut keys: Vec<(segment::SegmentRef, String)> = h
        .destination()
        .list(&format!("{}/generations/", h.root))
        .expect("list")
        .into_iter()
        .filter_map(|k| segment::parse_segment_key(&k).map(|r| (r, k)))
        .collect();
    keys.sort_by_key(|entry| entry.0);
    keys.into_iter().map(|(_, k)| k).collect()
}

fn generations_on(h: &Harness) -> std::collections::BTreeSet<String> {
    h.destination()
        .list(&format!("{}/generations/", h.root))
        .expect("list")
        .iter()
        .filter_map(|k| segment::generation_of_key(&h.root, k))
        .collect()
}

/// A clock that moves one second forward on every reading.
///
/// Makes the *order* in which the replicator reads time observable: each
/// artifact's timestamp says exactly which reading stamped it.
struct AdvancingClock(Mutex<DateTime<Utc>>);

impl ClockSource for AdvancingClock {
    fn now(&self) -> DateTime<Utc> {
        let mut current = self.0.lock().expect("clock");
        let reading = *current;
        *current = reading + chrono::Duration::seconds(1);
        reading
    }
}

/// Every artifact is stamped only once its contents are fenced.
///
/// The tick reads the `-wal`'s length, then reads the bytes: a transaction can
/// commit inside that window and land in the segment. A timestamp sampled at the
/// top of the tick would therefore date the segment *earlier* than a change it
/// actually carries, and a point-in-time restore to an instant in between would
/// replay that change — the one thing a PITR must never do. Snapshots have the
/// same shape: the checkpoint is what fences the database file they copy.
///
/// Both readings must land after the tick's first, and the snapshot's before the
/// segment's, which is the order the fences happen in.
#[test]
fn an_artifact_is_stamped_after_its_contents_are_fenced() {
    let mut h = Harness::new();
    let clock = Arc::new(AdvancingClock(Mutex::new(at(0))));
    h.replicator = Replicator::new(settings(&h.db), h.destination(), Arc::clone(&h.status))
        .with_clock(Arc::clone(&clock) as Arc<dyn ClockSource>);

    // A reading taken here is strictly earlier than every reading the tick that
    // follows will take, so "stamped later than this" means "stamped from inside
    // the tick", never from a value carried in from before it.
    let before_opening = clock.now();
    // The opening tick checkpoints first, so it publishes a base snapshot and
    // ships nothing; the write below is what the next tick turns into a segment.
    let opened = h.replicator.tick().expect("opening tick");
    assert!(opened.snapshot_taken, "{opened:?}");

    insert(&mut h.conn, &["alpha"]);
    let before_shipping = clock.now();
    let shipped = h.replicator.tick().expect("shipping tick");
    assert_eq!(shipped.segments, 1, "{shipped:?}");

    let generation = generations_on(&h)
        .into_iter()
        .next()
        .expect("one generation");
    let snapshot_ms = segment::parse_generation_id(&generation)
        .expect("generation id")
        .created_ms;
    let segment_ms = h
        .destination()
        .list(&segment::segments_prefix(&h.root, &generation))
        .expect("list")
        .iter()
        .filter_map(|key| segment::parse_segment_key(key))
        .map(|reference| reference.created_ms)
        .max()
        .expect("one segment");

    let opening = before_opening.timestamp_millis();
    assert!(
        snapshot_ms > opening,
        "the snapshot must be stamped after the checkpoint that fenced the database file, \
         from a reading taken inside the tick ({snapshot_ms} vs {opening})"
    );
    let shipping = before_shipping.timestamp_millis();
    assert!(
        segment_ms > shipping,
        "the segment must be stamped after its WAL bytes were read, from a reading taken \
         inside the tick that read them ({segment_ms} vs {shipping})"
    );
    assert!(
        segment_ms > snapshot_ms,
        "the fences happen in this order, so the stamps must too ({segment_ms} vs \
         {snapshot_ms})"
    );
}

#[test]
fn a_checkpoint_opens_the_next_wal_index_without_re_uploading_the_database() {
    let mut h = Harness::new();
    // A tiny WAL budget makes the checkpoint fire on the very next tick, while
    // the (default, one-hour) snapshot interval keeps the generation young.
    let mut tight = settings(&h.db);
    tight.max_wal_bytes = 1;
    h.rebuild(tight);

    prime(&mut h, 0);
    insert(&mut h.conn, &["index-zero"]);
    let first = tick_until_checkpointed(&mut h, 1);
    assert!(
        first.index_rotated,
        "a young generation must survive a checkpoint"
    );

    insert(&mut h.conn, &["index-one"]);
    let second = h.tick(30).expect("tick");
    assert!(
        !second.snapshot_taken,
        "re-uploading the whole database on every checkpoint is the write \
         amplification this split exists to avoid"
    );
    assert_eq!(second.segments, 1);

    assert_eq!(
        generations_on(&h).len(),
        1,
        "a checkpoint must not open a new generation while the old one is young"
    );
    let indexes: std::collections::BTreeSet<u32> = h
        .destination()
        .list(&format!("{}/generations/", h.root))
        .expect("list")
        .iter()
        .filter_map(|k| segment::parse_segment_key(k).map(|r| r.index))
        .collect();
    assert!(
        indexes.len() >= 2,
        "expected segments in at least two WAL indexes, got {indexes:?}"
    );

    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(values(&read_rows(&restored)), ["index-zero", "index-one"]);
}

#[test]
fn an_expired_generation_takes_a_fresh_base_snapshot_at_the_next_checkpoint() {
    let mut h = Harness::new();
    let mut tight = settings(&h.db);
    tight.max_wal_bytes = 1;
    tight.snapshot_interval = Duration::from_secs(1);
    h.rebuild(tight);

    prime(&mut h, 0);
    insert(&mut h.conn, &["gen-one"]);
    let checkpoint = tick_until_checkpointed(&mut h, 1);
    assert!(
        !checkpoint.index_rotated,
        "a generation past the snapshot interval must be replaced, not extended"
    );

    // The replacement base snapshot is taken on the next tick, from the database
    // file the checkpoint just completed.
    insert(&mut h.conn, &["gen-two"]);
    let next = h.tick(3_600).expect("tick");
    assert!(next.snapshot_taken, "a fresh base snapshot must follow");
    assert!(generations_on(&h).len() >= 2);

    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(values(&read_rows(&restored)), ["gen-one", "gen-two"]);
}

/// A replica missing its newest segments must be refused, not restored.
///
/// The chain walk catches a hole in the middle, because a later sequence
/// exposes it. A missing *tail* leaves a perfectly contiguous prefix, so
/// without the generation head a truncated replica restores "cleanly" — short
/// of its newest commits — and periodic verification agrees with it. That is
/// the silent case: an operator restores, sees a working database, and never
/// learns which commits are gone.
#[test]
fn a_replica_missing_its_newest_segments_is_refused() {
    let mut h = Harness::new();
    prime(&mut h, 0);

    insert(&mut h.conn, &["one"]);
    assert_eq!(h.tick(1).expect("tick").segments, 1);
    insert(&mut h.conn, &["two"]);
    assert_eq!(h.tick(2).expect("tick").segments, 1);

    // A latest restore is happy while the chain is whole.
    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(values(&read_rows(&restored)), ["one", "two"]);
    std::fs::remove_file(&restored).expect("clear the restore output");

    // Now lose the newest segment the way a lifecycle rule or a stray delete
    // would: the object goes, the head stays.
    let newest = segment_keys_on(&h)
        .into_iter()
        .next_back()
        .expect("a shipped segment");
    h.delete_object(&newest);

    let err = restore::restore(
        h.destination().as_ref(),
        &h.root,
        None,
        &h.replica_root
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("truncated.db"),
    )
    .expect_err("a truncated replica must be refused");
    let message = err.to_string();
    assert!(
        message.contains("missing its newest segments"),
        "the refusal must name the truncation, got: {message}"
    );
}

/// A host clock that steps backward must not hide the newest generation.
///
/// Generation ids order by their millisecond stamp, and a latest-restore takes
/// the greatest. If a backward clock step let the newer generation be stamped
/// lower, that restore would select the *older* one and silently drop every
/// write captured after the step — and this is not a contrived pairing:
/// `generation_expired` deliberately treats a negative elapsed duration as
/// expiry, so a rollback is exactly the moment a new generation gets opened.
#[test]
fn a_backward_clock_step_does_not_hide_the_newest_generation() {
    let mut h = Harness::new();
    let mut tight = settings(&h.db);
    tight.max_wal_bytes = 1;
    tight.snapshot_interval = Duration::from_secs(1);
    h.rebuild(tight);

    // Two generations at a high wall clock.
    prime(&mut h, 10_000);
    insert(&mut h.conn, &["before-the-step"]);
    tick_until_checkpointed(&mut h, 10_001);
    let before = h.tick(20_000).expect("tick");
    assert!(before.snapshot_taken, "the second generation should open");
    let after_two = generations_on(&h);
    assert!(after_two.len() >= 2, "two generations: {after_two:?}");

    // The host clock steps back an hour — NTP correction, a VM restored from a
    // snapshot, an operator fixing a wrong date — and a write lands after it.
    insert(&mut h.conn, &["after-the-step"]);
    tick_until_checkpointed(&mut h, 16_400);
    let rolled_back = h.tick(16_401).expect("tick");
    assert!(
        rolled_back.snapshot_taken,
        "a backward step expires the open generation, opening a new one"
    );

    let generations = generations_on(&h);
    let newest = generations.iter().next_back().expect("a generation");
    assert!(
        !after_two.contains(newest),
        "the generation opened after the clock step must sort newest, not behind \
         the ones that preceded it (generations: {generations:?})"
    );

    // The property that ordering exists to protect: the latest restore has to
    // carry the write that landed after the step.
    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(
        values(&read_rows(&restored)),
        ["before-the-step", "after-the-step"],
        "a latest restore must not regress past a backward clock step"
    );
}

#[test]
fn a_restore_spans_many_wal_indexes_in_one_generation() {
    let mut h = Harness::new();
    let mut tight = settings(&h.db);
    tight.max_wal_bytes = 1; // checkpoint (and rotate the index) on every tick
    h.rebuild(tight);

    prime(&mut h, 0);
    let mut expected = Vec::new();
    for round in 0..6 {
        let value = format!("row-{round}");
        insert(&mut h.conn, &[&value]);
        expected.push(value);
        tick_until_checkpointed(&mut h, (round + 1) * 100);
    }
    assert_eq!(
        generations_on(&h).len(),
        1,
        "six checkpoints must still be one generation"
    );

    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(values(&read_rows(&restored)), expected);
}

// ─── AC: safe against a live app ─────────────────────────────────────────────

#[test]
fn replication_never_blocks_or_corrupts_a_live_writer() {
    let mut h = Harness::new();
    let mut tight = settings(&h.db);
    // Checkpoint aggressively: the most contentious thing the replicator does.
    tight.max_wal_bytes = 4096;
    h.rebuild(tight);

    let mut expected = Vec::new();
    let mut checkpoints = 0;
    for round in 0..40 {
        let value = format!("row-{round}");
        insert(&mut h.conn, &[&value]);
        expected.push(value);
        // Interleave a replication tick with every write, the worst case for
        // contention on the single writer.
        let report = h
            .tick(round)
            .unwrap_or_else(|e| panic!("tick {round} failed: {e}"));
        if report.checkpointed {
            checkpoints += 1;
        }
    }
    assert!(
        checkpoints > 0,
        "the tight WAL budget should have forced checkpoints — with none, this test \
         proves nothing about the contended path"
    );

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
    prime(&mut h, 0);
    insert(&mut h.conn, &["a"]);
    h.tick(0).expect("tick");

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
    let err = h.tick(50).expect_err("no database");
    assert!(format!("{err}").contains("does not exist"), "{err}");
    let after = h.status.snapshot();
    assert_eq!(after.last_success_at, Some(at(0)), "lag must keep growing");
    assert_eq!(after.consecutive_failures, 1);
}

#[test]
fn verification_actually_restores_and_fails_loudly_on_a_damaged_replica() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["verified"]);
    h.tick(0).expect("tick");

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
    short.max_wal_bytes = 1; // checkpoint on every tick…
    short.snapshot_interval = Duration::from_secs(1); // …and open a new generation
    short.retention = Duration::from_secs(60);
    h.rebuild(short);

    for round in 0..5 {
        insert(&mut h.conn, &[&format!("row-{round}")]);
        h.tick(round * 100)
            .unwrap_or_else(|e| panic!("tick {round} failed: {e}"));
        h.tick(round * 100 + 1)
            .unwrap_or_else(|e| panic!("tick {round} rotation failed: {e}"));
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

// ─── The loop itself: its own timer, and the shutdown flush ──────────────────

/// A destination that takes its time, so "the upload does not block the writer"
/// is an assertion rather than a hope.
struct SlowDestination {
    inner: FileDestination,
    delay: Duration,
}

impl ReplicaDestination for SlowDestination {
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn put(&self, key: &str, body: &[u8]) -> Result<(), DestinationError> {
        std::thread::sleep(self.delay);
        self.inner.put(key, body)
    }
    fn put_file(&self, key: &str, path: &Path) -> Result<(), DestinationError> {
        std::thread::sleep(self.delay);
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
fn the_loop_ships_on_its_own_and_flushes_what_is_left_on_shutdown() {
    let mut h = Harness::new();
    let mut settings = settings(&h.db);
    settings.sync_interval = Duration::from_millis(50);
    let replicator = Replicator::new(settings, h.destination(), Arc::clone(&h.status));

    let shutdown = tokio_util::sync::CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let worker = std::thread::spawn(move || replicator.run(&loop_shutdown));

    // Wait for the loop to open a generation on its own timer, then write rows
    // it has not seen and stop it immediately: only the shutdown flush can get
    // them offsite.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while h.status.snapshot().generation.is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the replication loop never opened a generation"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    insert(&mut h.conn, &["written-just-before-shutdown"]);
    shutdown.cancel();
    worker
        .join()
        .expect("the replication thread must not panic");

    let snapshot = h.status.snapshot();
    assert!(snapshot.segments_shipped >= 1, "{snapshot:?}");
    assert!(snapshot.last_success_at.is_some(), "{snapshot:?}");

    h.destroy_database();
    let restored = h.restore_to(None);
    assert_eq!(
        values(&read_rows(&restored)),
        ["written-just-before-shutdown"],
        "a clean shutdown must not drop the last committed transaction"
    );
}

#[test]
fn a_writer_on_another_thread_is_not_blocked_by_a_slow_upload() {
    let mut h = Harness::with_destination(|dir| {
        Arc::new(SlowDestination {
            inner: FileDestination::new(dir.join("replica")).expect("destination"),
            // Far longer than any write takes, so a writer that serialized
            // behind uploads could not possibly finish inside the bound below.
            delay: Duration::from_millis(120),
        })
    });
    let db = h.db.clone();
    let mut settings = settings(&h.db);
    settings.sync_interval = Duration::from_millis(10);
    // Checkpoint constantly: the one thing the replicator does that touches
    // SQLite's write lock.
    settings.max_wal_bytes = 4096;
    let replicator = Replicator::new(settings, h.destination(), Arc::clone(&h.status));

    let shutdown = tokio_util::sync::CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let worker = std::thread::spawn(move || replicator.run(&loop_shutdown));

    // 40 writes on their own connection while the replicator uploads. Each
    // upload sleeps 120 ms; if writes serialized behind them this could not
    // finish in seconds.
    let started = std::time::Instant::now();
    let mut writer = open_app_db(&db);
    let mut expected = Vec::new();
    for round in 0..40 {
        let value = format!("row-{round}");
        sql_query(format!("INSERT INTO t (v) VALUES ('{value}')"))
            .execute(&mut writer)
            .unwrap_or_else(|e| panic!("write {round} was refused while replicating: {e}"));
        expected.push(value);
    }
    let elapsed = started.elapsed();
    drop(writer);

    shutdown.cancel();
    worker
        .join()
        .expect("the replication thread must not panic");

    assert!(
        elapsed < Duration::from_secs(3),
        "40 writes took {elapsed:?} — they were serialized behind the replicator's uploads"
    );
    assert_eq!(
        values(&read_rows(&h.db)),
        expected,
        "the live database is intact"
    );

    h.destroy_database();
    let restored = h.restore_to(None);
    let restored_rows = values(&read_rows(&restored));
    assert!(
        expected.starts_with(&restored_rows) || restored_rows == expected,
        "the replica must be a prefix of what was written, got {restored_rows:?}"
    );
    assert!(!restored_rows.is_empty(), "the replica must not be empty");
}

#[test]
fn a_periodic_verification_records_its_outcome_in_the_operator_status() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["verified"]);
    h.tick(0).expect("tick");

    let mut settings = settings(&h.db);
    settings.sync_interval = Duration::from_millis(50);
    settings.verify_interval = Some(Duration::from_millis(1));
    let replicator = Replicator::new(settings, h.destination(), Arc::clone(&h.status));

    let shutdown = tokio_util::sync::CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let worker = std::thread::spawn(move || replicator.run(&loop_shutdown));

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while h.status.snapshot().last_verified_at.is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the loop never recorded a verification: {:?}",
            h.status.snapshot()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    shutdown.cancel();
    worker.join().expect("the verification must not panic");

    let snapshot = h.status.snapshot();
    assert!(snapshot.last_verified_at.is_some());
    assert!(snapshot.last_verify_error.is_none(), "{snapshot:?}");
}

#[test]
fn a_failed_verification_reaches_the_status_the_health_indicator_reads() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["doomed"]);
    let destination = h.destination();

    let mut settings = settings(&h.db);
    settings.sync_interval = Duration::from_millis(50);
    settings.verify_interval = Some(Duration::from_millis(1));
    let replicator = Replicator::new(settings, Arc::clone(&destination), Arc::clone(&h.status));

    let shutdown = tokio_util::sync::CancellationToken::new();
    let loop_shutdown = shutdown.clone();
    let worker = std::thread::spawn(move || replicator.run(&loop_shutdown));

    // Let the loop settle on a generation and verify it once, so the corruption
    // below lands on the replica it is actually using.
    let settled = std::time::Instant::now() + Duration::from_secs(30);
    while h.status.snapshot().last_verified_at.is_none() {
        assert!(
            std::time::Instant::now() < settled,
            "the loop never verified a healthy replica: {:?}",
            h.status.snapshot()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Now break the replica behind the replicator's back.
    let snapshot_key = destination
        .list(&format!("{}/generations/", h.root))
        .expect("list")
        .into_iter()
        .find(|k| k.ends_with(segment::SNAPSHOT_OBJECT))
        .expect("snapshot exists");
    destination
        .put(&snapshot_key, b"not a gzip stream at all")
        .expect("put");

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while h.status.snapshot().last_verify_error.is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "a broken replica never produced a verification failure: {:?}",
            h.status.snapshot()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    shutdown.cancel();
    worker.join().expect("the verification must not panic");

    // The health indicator turns that into a DOWN, which is what #1610's
    // alerter escalates.
    let indicator = ReplicationHealthIndicator::new(
        Arc::clone(&h.status),
        HealthThresholds {
            lag_alert_after: Duration::from_secs(30),
            startup_grace: Duration::from_secs(60),
        },
        Utc::now(),
    );
    let output = indicator.evaluate(&h.status.snapshot(), Utc::now());
    assert_eq!(output.status, HealthStatus::Down);
    assert!(output.details.contains_key("verification_error"));
}

#[test]
fn the_replication_indicator_registers_and_reports_down_through_the_registry() {
    // The registry is what `/actuator/health` serves and what #1610's alerter
    // sweeps every cycle, so registration under the documented name — in the
    // `HealthOnly` group, so a lagging replica never pulls the process out of
    // the load balancer — is the joint that makes a verification failure
    // alertable rather than merely visible.
    let status = Arc::new(ReplicationStatus::new("file:///replicas"));
    let indicator = Arc::new(ReplicationHealthIndicator::new(
        Arc::clone(&status),
        HealthThresholds {
            lag_alert_after: Duration::from_secs(30),
            startup_grace: Duration::from_secs(60),
        },
        Utc::now(),
    ));
    let registry = HealthIndicatorRegistry::new();
    registry
        .register(
            autumn_web::replication::INDICATOR_NAME,
            IndicatorGroup::HealthOnly,
            indicator,
        )
        .expect("register the replication indicator");

    status.record_tick_ok(0, Utc::now());
    status.record_verification(Err("integrity check failed".to_owned()), Utc::now());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let results = runtime.block_on(registry.run_all());
    let result = results
        .iter()
        .find(|r| r.name == autumn_web::replication::INDICATOR_NAME)
        .expect("the indicator is registered under its documented name");
    assert!(
        !result.output.status.is_healthy(),
        "a replica that failed verification must report non-healthy: {:?}",
        result.output.status
    );
    assert_eq!(result.group, IndicatorGroup::HealthOnly);

    // …and recovers once a verification succeeds again.
    status.record_verification(Ok(()), Utc::now());
    let recovered = runtime.block_on(registry.run_all());
    assert!(
        recovered.iter().all(|r| r.output.status.is_healthy()),
        "the indicator must recover so the alerter can resolve the incident"
    );
}

// ─── Point-in-time across generations, and the integrity backstop ───────────

#[test]
fn a_point_in_time_restore_picks_the_right_generation_out_of_several() {
    let mut h = Harness::new();
    let mut rotating = settings(&h.db);
    rotating.max_wal_bytes = 1;
    rotating.snapshot_interval = Duration::from_secs(1);
    h.rebuild(rotating);

    // Each round is 3600 s apart, so every round opens its own generation.
    let mut expected_after = Vec::new();
    for round in 0..4 {
        let value = format!("row-{round}");
        insert(&mut h.conn, &[&value]);
        expected_after.push(value);
        h.tick(round * 3_600)
            .unwrap_or_else(|e| panic!("tick {round} failed: {e}"));
        // A second tick opens the replacement generation from the database file
        // the checkpoint just completed.
        h.tick(round * 3_600 + 1)
            .unwrap_or_else(|e| panic!("tick {round} rotation failed: {e}"));
    }
    assert!(
        generations_on(&h).len() >= 3,
        "this test needs several generations, got {:?}",
        generations_on(&h)
    );

    h.destroy_database();

    // A target inside the middle of the history must select the generation that
    // opened at or before it, not simply the newest one.
    let middle = h.restore_to(Some(at(2 * 3_600 + 60)));
    let rows = values(&read_rows(&middle));
    assert!(
        rows.contains(&"row-0".to_owned()) && rows.contains(&"row-2".to_owned()),
        "expected everything up to round 2, got {rows:?}"
    );
    assert!(
        !rows.contains(&"row-3".to_owned()),
        "a later round must not appear in an earlier point-in-time restore: {rows:?}"
    );

    let latest = h.restore_to(None);
    assert_eq!(values(&read_rows(&latest)), expected_after);
}

#[test]
fn a_snapshot_corrupted_together_with_its_digest_is_still_refused() {
    use std::io::Write as _;

    let mut h = Harness::new();
    insert(&mut h.conn, &["one", "two", "three"]);
    h.tick(0).expect("tick");

    let destination = h.destination();
    let keys = destination
        .list(&format!("{}/generations/", h.root))
        .expect("list");
    let snapshot_key = keys
        .iter()
        .find(|k| k.ends_with(segment::SNAPSHOT_OBJECT))
        .expect("snapshot")
        .clone();
    let meta_key = keys
        .iter()
        .find(|k| k.ends_with(segment::SNAPSHOT_META_OBJECT))
        .expect("marker")
        .clone();

    // Inflate the snapshot, corrupt its middle, re-gzip it, and rewrite the
    // metadata so the SHA-256 and length still match — the one case digests
    // cannot catch, and the reason `PRAGMA integrity_check` is a backstop.
    let compressed = destination.get(&snapshot_key).expect("get snapshot");
    let mut plain = Vec::new();
    std::io::copy(
        &mut flate2::read::GzDecoder::new(&compressed[..]),
        &mut plain,
    )
    .expect("inflate");
    let len = plain.len();
    for byte in plain.iter_mut().skip(len / 2).take(1024) {
        *byte ^= 0xFF;
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&plain).expect("compress");
    destination
        .put(&snapshot_key, &encoder.finish().expect("finish"))
        .expect("put snapshot");

    let mut meta: serde_json::Value =
        serde_json::from_slice(&destination.get(&meta_key).expect("get meta")).expect("parse");
    meta["sha256"] = serde_json::Value::String(segment::sha256_hex(&plain));
    meta["uncompressed_len"] = serde_json::Value::from(plain.len() as u64);
    destination
        .put(&meta_key, &serde_json::to_vec(&meta).expect("encode"))
        .expect("put meta");

    let output = h.replica_root.join("../corrupt.db");
    let err = restore::restore(destination.as_ref(), &h.root, None, &output)
        .expect_err("a corrupt database must be refused even when its digest matches");
    let rendered = format!("{err}");
    assert!(
        rendered.contains("integrity") || rendered.contains("replay the WAL"),
        "{rendered}"
    );
    assert!(
        !output.exists(),
        "a refused restore must not publish the corrupt database"
    );
}

#[test]
fn a_failed_publish_leaves_the_existing_databases_wal_intact() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["one", "two", "three"]);
    h.tick(0).expect("tick");

    // Publishing over an existing *directory* fails: neither the rename nor the
    // cross-filesystem copy fallback can replace one with a file. That is the
    // cheapest deterministic way to fail the publish after the replica has been
    // fully rebuilt and verified in staging.
    let output = h.replica_root.join("../occupied.db");
    std::fs::create_dir_all(&output).expect("occupy the output path");

    // Stands in for a live database's un-checkpointed commits: the bytes that
    // exist *only* in the sidecar until something checkpoints them.
    let wal_path = autumn_web::replication::wal::wal_path(&output);
    let commits = b"the existing database's newest commits";
    std::fs::write(&wal_path, commits).expect("write the existing WAL");

    let err = restore::restore(h.destination().as_ref(), &h.root, None, &output)
        .expect_err("publishing onto a directory must fail");

    // The contract is failure atomicity: an error leaves what it found. Before
    // the sidecars were displaced rather than deleted, this restore returned an
    // error having already destroyed the database's newest commits.
    assert_eq!(
        std::fs::read(&wal_path).expect("the existing WAL must survive a failed publish"),
        commits,
        "a failed publish ({err}) must not consume the existing database's WAL",
    );
    assert!(
        !autumn_web::replication::wal::shm_path(&output).exists(),
        "no sidecar should be invented for a database that had none"
    );
    assert!(
        !std::fs::exists(format!("{}.displaced", wal_path.display())).unwrap_or(false),
        "the parked sidecar must not be left behind"
    );
}

#[test]
fn a_sidecar_that_cannot_be_displaced_refuses_the_restore() {
    let mut h = Harness::new();
    insert(&mut h.conn, &["one", "two", "three"]);
    h.tick(0).expect("tick");

    let output = h.replica_root.join("../blocked.db");
    let existing = b"the existing database";
    std::fs::write(&output, existing).expect("existing database");
    let wal_path = autumn_web::replication::wal::wal_path(&output);
    let commits = b"commits that live only in the WAL";
    std::fs::write(&wal_path, commits).expect("existing WAL");

    // A directory where the sidecar must be parked: `remove_file` cannot clear
    // it, so the sidecar cannot be moved out of the way. Skipping it would
    // publish the restored database next to a foreign WAL, and SQLite would
    // recover that WAL over it — losing the restore, not just the sidecar.
    std::fs::create_dir_all(format!("{}.displaced", wal_path.display())).expect("block the park");

    let err = restore::restore(h.destination().as_ref(), &h.root, None, &output)
        .expect_err("a sidecar that cannot be displaced must refuse the restore");

    assert_eq!(
        std::fs::read(&output).expect("the existing database must still be there"),
        existing,
        "publishing must not proceed past a sidecar that could not be displaced ({err})"
    );
    assert_eq!(
        std::fs::read(&wal_path).expect("the existing WAL must still be there"),
        commits,
        "a refused restore must leave the WAL exactly where it found it"
    );
}
