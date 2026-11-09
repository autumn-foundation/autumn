//! `ReplicationSnapshot::replicated_through`: the instant up to which every
//! commit is in the replica.
//!
//! A hook clock commits a row inside one clock read of a tick. The suite
//! moves that commit to each clock read of the tick in turn. A commit that
//! lands after the caught-up check must not be covered.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::replication::{
    FileDestination, ReplicaDestination, ReplicationError, ReplicationSettings, ReplicationStatus,
    Replicator, restore, segment,
};
use autumn_web::time::ClockSource;
use chrono::{DateTime, TimeZone as _, Utc};
use diesel::connection::SimpleConnection as _;
use diesel::sql_types::Text;
use diesel::{Connection as _, QueryableByName, RunQueryDsl as _, SqliteConnection, sql_query};

#[derive(QueryableByName)]
struct Row {
    #[diesel(sql_type = Text)]
    v: String,
}

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_700_000_000 + secs, 0)
        .single()
        .expect("timestamp")
}

/// Open the database as the pool does with replication on.
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

fn insert(conn: &mut SqliteConnection, value: &str) {
    sql_query(format!("INSERT INTO t (v) VALUES ('{value}')"))
        .execute(conn)
        .expect("insert");
}

type Hook = Box<dyn FnOnce(DateTime<Utc>) + Send>;

/// A clock that moves one second on each read. It can run a hook inside
/// read number `n`, before that read returns.
struct HookClock {
    now: Mutex<DateTime<Utc>>,
    reads: AtomicUsize,
    hook: Mutex<Option<(usize, Hook)>>,
}

impl HookClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            now: Mutex::new(at(0)),
            reads: AtomicUsize::new(0),
            hook: Mutex::new(None),
        })
    }

    /// The current instant, without a read.
    fn peek(&self) -> DateTime<Utc> {
        *self.now.lock().expect("clock")
    }

    fn set(&self, now: DateTime<Utc>) {
        *self.now.lock().expect("clock") = now;
    }

    /// Run `hook` inside the `n`th read from now (0 is the next read).
    fn arm(&self, n: usize, hook: Hook) {
        let at = self.reads.load(Ordering::SeqCst) + n;
        *self.hook.lock().expect("hook") = Some((at, hook));
    }
}

impl ClockSource for HookClock {
    fn now(&self) -> DateTime<Utc> {
        let value = {
            let mut now = self.now.lock().expect("clock");
            *now += chrono::Duration::seconds(1);
            *now
        };
        let read = self.reads.fetch_add(1, Ordering::SeqCst);
        let hook = {
            let mut slot = self.hook.lock().expect("hook");
            if slot.as_ref().is_some_and(|(n, _)| *n == read) {
                slot.take().map(|(_, hook)| hook)
            } else {
                None
            }
        };
        if let Some(hook) = hook {
            hook(value);
        }
        value
    }
}

fn settings(db: &Path) -> ReplicationSettings {
    ReplicationSettings {
        database_path: db.to_path_buf(),
        root: segment::root_prefix(None, "test"),
        sync_interval: Duration::from_secs(1),
        snapshot_interval: Duration::from_hours(1),
        max_wal_bytes: 16 * 1024 * 1024,
        retention: Duration::from_hours(7 * 24),
        verify_interval: None,
    }
}

struct Harness {
    dir: tempfile::TempDir,
    db: PathBuf,
    conn: SqliteConnection,
    replicator: Replicator,
    status: Arc<ReplicationStatus>,
    destination: Arc<dyn ReplicaDestination>,
    root: String,
    clock: Arc<HookClock>,
    restores: usize,
}

impl Harness {
    fn new() -> Self {
        Self::with(|settings| settings)
    }

    fn with(tune: impl FnOnce(ReplicationSettings) -> ReplicationSettings) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("app.db");
        let mut conn = open_app_db(&db);
        conn.batch_execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT NOT NULL);")
            .expect("create table");
        let destination: Arc<dyn ReplicaDestination> =
            Arc::new(FileDestination::new(dir.path().join("replica")).expect("destination"));
        let status = Arc::new(ReplicationStatus::new(destination.describe()));
        let settings = tune(settings(&db));
        let root = settings.root.clone();
        let clock = HookClock::new();
        let replicator = Replicator::new(settings, Arc::clone(&destination), Arc::clone(&status))
            .with_clock(Arc::clone(&clock) as Arc<dyn ClockSource>);
        Self {
            dir,
            db,
            conn,
            replicator,
            status,
            destination,
            root,
            clock,
            restores: 0,
        }
    }

    fn through(&self) -> Option<DateTime<Utc>> {
        self.status.snapshot().replicated_through
    }

    /// The rows a restore of the replica holds now.
    fn replica_rows(&mut self) -> Vec<String> {
        self.restores += 1;
        let output = self
            .dir
            .path()
            .join(format!("restored-{}.db", self.restores));
        restore::restore(self.destination.as_ref(), &self.root, None, &output).expect("restore");
        let mut conn =
            SqliteConnection::establish(&output.to_string_lossy()).expect("open restored");
        sql_query("SELECT v FROM t ORDER BY id")
            .load::<Row>(&mut conn)
            .expect("select")
            .into_iter()
            .map(|row| row.v)
            .collect()
    }
}

/// What one sweep step saw.
struct Step {
    /// The hook ran inside the tick.
    fired: bool,
    /// `last_success_at` covers the late row, and the replica lacks it.
    old_signal_lies: bool,
}

/// Commit a row inside clock read `n` of one tick, then check the claim.
fn commit_inside_read(n: usize) -> Step {
    let mut h = Harness::new();
    insert(&mut h.conn, "base");
    h.replicator.tick().expect("first tick");
    insert(&mut h.conn, "pre");
    let pre_at = h.clock.peek();
    h.replicator.tick().expect("second tick");
    let before = h.through();
    assert!(
        before.is_some_and(|through| through >= pre_at),
        "a caught-up tick covers what was committed before it: {before:?} vs {pre_at:?}"
    );

    let late: Arc<Mutex<Option<DateTime<Utc>>>> = Arc::new(Mutex::new(None));
    let db = h.db.clone();
    let seen = Arc::clone(&late);
    h.clock.arm(
        n,
        Box::new(move |now| {
            let mut conn = open_app_db(&db);
            insert(&mut conn, "late");
            *seen.lock().expect("late") = Some(now);
        }),
    );
    h.replicator.tick().expect("hooked tick");
    let late_at = *late.lock().expect("late");
    let snapshot = h.status.snapshot();
    let rows = h.replica_rows();

    let through = snapshot.replicated_through;
    assert!(
        through >= before,
        "the bound never moves back in one generation: {through:?} < {before:?}"
    );
    for (row, committed_at) in [("pre", Some(pre_at)), ("late", late_at)] {
        let Some(committed_at) = committed_at else {
            continue;
        };
        if through.is_some_and(|through| committed_at <= through) {
            assert!(
                rows.iter().any(|v| v == row),
                "read {n}: {row} committed at {committed_at} is at or before \
                 replicated_through {through:?}, but the replica lacks it: {rows:?}"
            );
        }
    }

    // The next tick ships the late row and covers it.
    h.replicator.tick().expect("catch-up tick");
    if let Some(late_at) = late_at {
        assert!(
            h.through().is_some_and(|through| through >= late_at),
            "read {n}: the catch-up tick covers the late row"
        );
        assert!(h.replica_rows().iter().any(|v| v == "late"));
    }

    Step {
        fired: late_at.is_some(),
        old_signal_lies: late_at.is_some_and(|late_at| {
            snapshot.last_success_at.is_some_and(|last| last >= late_at)
                && !rows.iter().any(|v| v == "late")
        }),
    }
}

#[test]
fn a_commit_after_the_caught_up_check_is_not_covered() {
    let steps: Vec<Step> = (0..10).map(commit_inside_read).collect();
    assert!(
        steps.iter().filter(|step| step.fired).count() >= 3,
        "the sweep reaches every read of a tick"
    );
    assert!(
        steps.iter().any(|step| step.old_signal_lies),
        "the sweep must hit the gap that last_success_at covers"
    );
}

#[test]
fn replicated_through_never_moves_backwards_in_one_generation() {
    let mut h = Harness::new();
    insert(&mut h.conn, "a");
    h.clock.set(at(100));
    h.replicator.tick().expect("tick");
    let first = h.through().expect("a caught-up tick sets the bound");
    let generation = h.status.snapshot().generation;

    // The clock steps back. The stamp of this tick is lower.
    insert(&mut h.conn, "b");
    h.clock.set(at(90));
    h.replicator.tick().expect("tick");
    assert_eq!(h.status.snapshot().generation, generation);
    assert_eq!(h.through(), Some(first), "the bound keeps its maximum");
}

#[test]
fn a_new_generation_starts_a_new_bound() {
    let mut h = Harness::with(|settings| ReplicationSettings {
        snapshot_interval: Duration::from_secs(60),
        ..settings
    });
    insert(&mut h.conn, "a");
    h.clock.set(at(100));
    h.replicator.tick().expect("tick");
    let old_generation = h.status.snapshot().generation;
    assert!(h.through().is_some_and(|through| through > at(100)));

    // A backward step expires the generation at its next checkpoint.
    h.clock.set(at(50));
    h.replicator
        .tick()
        .expect("tick that retires the generation");
    h.clock.set(at(60));
    h.replicator
        .tick()
        .expect("tick that opens a new generation");
    let snapshot = h.status.snapshot();
    assert_ne!(
        snapshot.generation, old_generation,
        "a new generation opened"
    );
    let through = snapshot
        .replicated_through
        .expect("the new generation is caught up");
    assert!(
        through < at(100),
        "the bound of the new generation does not carry the old maximum: {through}"
    );
    assert!(h.replica_rows().iter().any(|v| v == "a"));
}

#[test]
fn a_fenced_replicator_has_no_bound() {
    let mut h = Harness::new();
    h.replicator.set_epoch(Some(1));
    insert(&mut h.conn, "a");
    h.replicator.tick().expect("tick");
    assert!(h.through().is_some());

    // A newer owner writes its epoch marker. The next fence check stops us.
    h.destination
        .put(&segment::epoch_marker_key(&h.root, 2), b"{}")
        .expect("marker");
    h.clock.set(h.clock.peek() + chrono::Duration::hours(1));
    let fenced = h.replicator.tick();
    assert!(
        matches!(fenced, Err(ReplicationError::Fenced { .. })),
        "{fenced:?}"
    );
    assert_eq!(h.through(), None, "a fenced replica claims nothing");
}

#[test]
fn a_new_epoch_clears_the_bound() {
    let mut h = Harness::new();
    insert(&mut h.conn, "a");
    h.replicator.tick().expect("tick");
    assert!(h.through().is_some());
    h.replicator.set_epoch(Some(7));
    assert_eq!(h.through(), None, "the next generation has not shipped yet");
    h.replicator.tick().expect("tick");
    assert!(h.through().is_some());
}

#[test]
fn a_tick_with_unshipped_commits_does_not_advance_the_bound() {
    let mut h = Harness::new();
    insert(&mut h.conn, "a");
    h.replicator.tick().expect("tick");
    let first = h.through();
    // The destination goes away with a commit not shipped.
    insert(&mut h.conn, "b");
    std::fs::remove_dir_all(h.dir.path().join("replica")).expect("remove");
    std::fs::write(h.dir.path().join("replica"), b"not a directory").expect("block");
    assert!(h.replicator.tick().is_err(), "the destination is gone");
    assert_eq!(
        h.through(),
        first,
        "a failed tick keeps the older, true bound"
    );
}

mod props {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Op {
        Insert,
        Tick,
        /// A tick with a commit inside clock read `n`.
        TickWithCommitInside(usize),
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            Just(Op::Insert),
            Just(Op::Tick),
            (0_usize..8).prop_map(Op::TickWithCommitInside),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 16,
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        /// A restore elsewhere holds every row committed at or before the
        /// bound, whatever order the writes and ticks come in.
        #[test]
        fn a_restore_holds_every_write_at_or_before_the_bound(
            ops in proptest::collection::vec(op(), 1..10),
        ) {
            let mut h = Harness::new();
            let committed: Arc<Mutex<Vec<(String, DateTime<Utc>)>>> =
                Arc::new(Mutex::new(Vec::new()));
            let mut last = None;
            for (i, op) in ops.iter().enumerate() {
                match op {
                    Op::Insert => {
                        let row = format!("row-{i}");
                        insert(&mut h.conn, &row);
                        committed.lock().expect("rows").push((row, h.clock.peek()));
                    }
                    Op::Tick | Op::TickWithCommitInside(_) => {
                        if let Op::TickWithCommitInside(n) = op {
                            let (db, rows, row) =
                                (h.db.clone(), Arc::clone(&committed), format!("row-{i}"));
                            h.clock.arm(*n, Box::new(move |now| {
                                insert(&mut open_app_db(&db), &row);
                                rows.lock().expect("rows").push((row, now));
                            }));
                        }
                        h.replicator.tick().expect("tick");
                        let through = h.through();
                        prop_assert!(through >= last, "the bound moved back");
                        last = through;
                        let Some(through) = through else { continue };
                        let restored = h.replica_rows();
                        for (row, at) in committed.lock().expect("rows").iter() {
                            prop_assert!(
                                *at > through || restored.contains(row),
                                "{row} at {at} is covered by {through} but not restored"
                            );
                        }
                    }
                }
            }
        }
    }
}
