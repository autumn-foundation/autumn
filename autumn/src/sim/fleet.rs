//! Framework scenarios for the seed sweep (issue #3067).
//!
//! Each scenario mounts two or three replicas on one `SQLite` database and
//! drives a framework coordination primitive under seeded faults:
//!
//! - `jobs`: the durable `SQLite` job queue. Faults: replica crash and restart,
//!   database session loss, mid-query errors and commit ambiguity on
//!   `autumn_jobs`. Invariants: no job runs on two replicas at once, and every
//!   job finishes.
//! - `scheduler`: the `SQLite` lease scheduler with a cron task and a
//!   fixed-delay task. Faults: clock offsets, drift and steps, crash and
//!   restart, session loss, faults on the lease table. Invariant: each tick
//!   runs at most once.
//! - `lock`: the `SQLite` [`Lock`](crate::lock::Lock) under session loss.
//!   Invariant: two holders overlap only after the first holder lost its
//!   session long enough for its lease to lapse. The `SQLite` lock gives no
//!   loss signal, so that overlap is its documented limit.
//!
//! Some jobs run longer than the visibility timeout. The #3051 heartbeat
//! renews their claims, and stops a run whose claim it cannot renew.
//!
//! [`run`] builds a [`runtime`](super::runtime), runs one scenario for one
//! seed, and returns its [`Trace`]. Only the `sim-sweep` binary and the sim
//! tests use this module. Its API can change; semver does not cover it.

#![allow(
    clippy::future_not_send,
    reason = "a fleet run holds the substrate's SQLite connection across awaits, on the current-thread sim runtime only"
)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use chrono::TimeDelta;
use serde_json::{Value, json};

use super::substrate::SqliteSubstrate;
use super::trace::{Trace, capture};
use super::{Replica, Sim};
use crate::config::{AutumnConfig, SchedulerBackend};
use crate::entropy::{Entropy, SeededEntropy};
use crate::state::AppState;
use crate::test::TestApp;
use crate::{AutumnResult, always, sometimes};

/// The scenario names [`run`] accepts.
pub const SCENARIOS: &[&str] = &["jobs", "scheduler", "lock"];

type ScenarioFuture<'a> = Pin<Box<dyn Future<Output = ()> + 'a>>;

/// Run the scenario `scenario` for `seed` on a fresh sim runtime.
///
/// # Errors
///
/// Returns the panic message when an invariant fails, or an error for an
/// unknown scenario name.
pub fn run(scenario: &str, seed: u64) -> Result<Trace, String> {
    let body: for<'a> fn(&'a mut Sim) -> ScenarioFuture<'a> = match scenario {
        "jobs" => |sim| Box::pin(jobs(sim)),
        "scheduler" => |sim| Box::pin(scheduler(sim)),
        "lock" => |sim| Box::pin(lock(sim)),
        other => {
            return Err(format!(
                "unknown fleet scenario `{other}`; the scenarios are {SCENARIOS:?}"
            ));
        }
    };
    let runtime = super::runtime().map_err(|error| format!("sim runtime: {error}"))?;
    let (outcome, trace) = runtime.block_on(async move {
        let mut sim = Sim::from_seed(seed);
        // Catch the panic inside the capture, so the trace survives it.
        capture(futures::FutureExt::catch_unwind(
            std::panic::AssertUnwindSafe(body(&mut sim)),
        ))
        .await
    });
    let give_ups = super::gate::give_ups();
    match outcome {
        Ok(()) if give_ups > 0 => Err(format!(
            "{give_ups} database operation(s) ran without their gate turn, so this run is \
             not sure to replay"
        )),
        Ok(()) => Ok(trace),
        Err(panic) => {
            let reason = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "the scenario panicked".to_owned());
            let tail = trace.lines().len().saturating_sub(FAILURE_TRACE_LINES);
            Err(format!(
                "{reason}\nthe last trace lines:\n{}",
                trace.lines()[tail..].join("\n")
            ))
        }
    }
}

/// How many trace lines a failure shows.
const FAILURE_TRACE_LINES: usize = 60;

// ── Shared plumbing ──────────────────────────────────────────────────────────

/// One timed step of a scenario plan.
#[derive(Debug, Clone)]
enum Step {
    Mount(&'static str),
    Kill(&'static str),
    Restart(&'static str),
    LoseSession(&'static str),
    RestoreSession(&'static str),
    MidQuery(&'static str, &'static str, f64),
    CommitAmbiguity(&'static str, &'static str, f64),
    ClearFaults(&'static str),
    JumpClock(&'static str, TimeDelta),
    /// Send a GET to a replica, through its `Db` extractor route.
    Get(&'static str, &'static str),
}

/// A seeded draw source for one scenario.
struct Draw(std::sync::Arc<dyn Entropy>);

impl Draw {
    fn new(seed: u64, purpose: &str) -> Self {
        Self(SeededEntropy::shared(super::derive_seed(seed, purpose)))
    }

    /// A value in `[low, high]`.
    fn between(&self, low: u64, high: u64) -> u64 {
        low + self.0.next_u64() % (high - low + 1)
    }

    fn millis(&self, low: u64, high: u64) -> Duration {
        Duration::from_millis(self.between(low, high))
    }

    /// `true` with probability `1 / n`.
    fn one_in(&self, n: u64) -> bool {
        self.0.next_u64().is_multiple_of(n)
    }

    fn pick<T: Copy>(&self, items: &[T]) -> T {
        let len = u64::try_from(items.len()).unwrap_or(1);
        let index = usize::try_from(self.0.next_u64() % len).unwrap_or(0);
        items[index]
    }
}

/// A scenario's database and app shape.
struct Fleet {
    substrate: SqliteSubstrate,
    replicas: Vec<Replica>,
    app: fn(&str) -> TestApp,
}

impl Fleet {
    fn new(replicas: Vec<Replica>, app: fn(&str) -> TestApp) -> Self {
        Self {
            substrate: SqliteSubstrate::new().expect("fleet: SQLite substrate"),
            replicas,
            app,
        }
    }

    fn build(&self, sim: &Sim, name: &str) -> TestApp {
        let pool = self
            .substrate
            .replica_pool(&sim.db_link(name))
            .expect("fleet: replica pool");
        (self.app)(name).with_db(pool)
    }

    fn apply(&self, sim: &mut Sim, step: &Step) {
        tracing::info!(step = ?step, "fleet step");
        match *step {
            Step::Mount(name) => {
                let spec = self
                    .replicas
                    .iter()
                    .find(|replica| replica.name() == name)
                    .cloned()
                    .unwrap_or_else(|| Replica::named(name));
                let app = self.build(sim, name);
                sim.mount_replica(spec, app);
            }
            Step::Kill(name) => {
                // A crash stops the replica's job runs: they no longer count
                // as running.
                note_crash(name);
                sim.kill_replica(name);
            }
            Step::Restart(name) => {
                let app = self.build(sim, name);
                sim.restart_replica(name, app);
            }
            Step::LoseSession(name) => sim.db_link(name).lose_session(),
            Step::RestoreSession(name) => sim.db_link(name).restore_session(),
            Step::MidQuery(name, table, p) => sim.db_link(name).mid_query_errors(table, p),
            Step::CommitAmbiguity(name, table, p) => {
                sim.db_link(name).commit_ambiguity(table, p);
            }
            Step::ClearFaults(name) => sim.db_link(name).clear_faults(),
            Step::JumpClock(name, by) => sim.step_replica_clock(name, by),
            // `play` sends requests: they need an await.
            Step::Get(..) => {}
        }
    }

    /// Run `plan` in time order, then run on until `end` (if it is later).
    async fn play(&self, sim: &mut Sim, mut plan: Vec<(Duration, Step)>, end: Duration) {
        plan.sort_by_key(|(at, _)| *at);
        let mut now = Duration::ZERO;
        for (at, step) in &plan {
            if *at > now {
                sim.run_for(at.saturating_sub(now)).await;
                now = *at;
            }
            if let Step::Get(name, path) = *step {
                if let Some(client) = sim.try_replica(name) {
                    let response = client.get(path).send().await;
                    tracing::info!(replica = name, path, status = %response.status, body = %response.text(), "fleet request");
                }
                continue;
            }
            self.apply(sim, step);
        }
        if end > now {
            sim.run_for(end.saturating_sub(now)).await;
        }
    }

    /// End every fault and bring every replica back.
    fn heal(&self, sim: &mut Sim) {
        for replica in &self.replicas {
            let name = replica.name();
            let link = sim.db_link(name);
            link.clear_faults();
            link.restore_session();
            if sim.try_replica(name).is_none() {
                let app = self.build(sim, name);
                sim.restart_replica(name, app);
            }
        }
    }
}

/// The replica a handler runs on.
fn replica_of(state: &AppState) -> String {
    state
        .extension::<AutumnConfig>()
        .and_then(|config| config.scheduler.replica_id.clone())
        .unwrap_or_default()
}

/// A replica config with the `SQLite` job queue and lease scheduler.
fn config(name: &str) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    "sqlite".clone_into(&mut config.jobs.backend);
    config.jobs.workers = 1;
    config.jobs.max_attempts = JOB_ATTEMPTS;
    config.jobs.initial_backoff_ms = 10;
    config.jobs.sqlite.visibility_timeout_ms = JOB_VISIBILITY_TIMEOUT_MS;
    config.jobs.sqlite.poll_interval_ms = 100;
    config.scheduler.backend = SchedulerBackend::Sqlite;
    config.scheduler.lease_ttl_secs = 2;
    config.scheduler.replica_id = Some(name.to_owned());
    config
}

// ── jobs ─────────────────────────────────────────────────────────────────────

const JOB: &str = "autumn_fleet_job";
const JOB_ATTEMPTS: u32 = 50;
const JOB_VISIBILITY_TIMEOUT_MS: u64 = 2_000;

/// What the job handlers saw, on this thread.
#[derive(Debug, Default)]
struct JobsLog {
    /// Job id to the replica that runs it now.
    running: BTreeMap<u64, String>,
    started: BTreeMap<u64, u32>,
    finished: BTreeMap<u64, u32>,
    replicas: BTreeSet<String>,
    /// Runs a crash stopped.
    stopped: u32,
    /// Runs that the job worker stopped (not by a crash), for example on a
    /// lost claim.
    cancelled: u32,
    /// Finished runs that were longer than the visibility timeout.
    long_finished: u32,
    overlaps: Vec<String>,
}

thread_local! {
    static JOBS: RefCell<JobsLog> = RefCell::default();
}

fn fleet_job(
    state: AppState,
    payload: Value,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
    Box::pin(async move {
        let id = payload["id"].as_u64().unwrap_or_default();
        let ms = payload["ms"].as_u64().unwrap_or_default();
        let replica = replica_of(&state);
        tracing::info!(job = id, replica = %replica, "fleet job started");
        JOBS.with(|log| {
            let mut log = log.borrow_mut();
            if let Some(other) = log.running.get(&id) {
                let overlap = format!("job {id} started on {replica} while it runs on {other}");
                log.overlaps.push(overlap);
            }
            log.running.insert(id, replica.clone());
            *log.started.entry(id).or_default() += 1;
            log.replicas.insert(replica.clone());
        });
        let mut run = RunGuard {
            id,
            replica: replica.clone(),
            finished: false,
        };
        tokio::time::sleep(Duration::from_millis(ms)).await;
        run.finished = true;
        JOBS.with(|log| {
            let mut log = log.borrow_mut();
            log.running.remove(&id);
            *log.finished.entry(id).or_default() += 1;
            if ms > JOB_VISIBILITY_TIMEOUT_MS {
                log.long_finished += 1;
            }
        });
        tracing::info!(job = id, replica = %replica, "fleet job finished");
        Ok(())
    })
}

/// Removes the run from `running` if the job worker drops the run before it
/// finishes. Example: the heartbeat lost the claim.
struct RunGuard {
    id: u64,
    replica: String,
    finished: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // The thread-local can be gone when the runtime drops its tasks.
        let _ = JOBS.try_with(|log| {
            let mut log = log.borrow_mut();
            if log.running.get(&self.id) == Some(&self.replica) {
                log.running.remove(&self.id);
                log.cancelled += 1;
            }
        });
    }
}

/// A request-path read of the job table, so the scenario mixes `Db`
/// extractor queries with the job runtime's own.
async fn open_jobs_route(mut db: crate::db::Db) -> String {
    use diesel_async::RunQueryDsl as _;

    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    diesel::sql_query("SELECT COUNT(*) AS n FROM autumn_jobs WHERE status <> 'completed'")
        .get_result::<Count>(&mut *db)
        .await
        .map_or_else(
            |error| format!("error: {error}"),
            |count| count.n.to_string(),
        )
}

fn jobs_app(name: &str) -> TestApp {
    TestApp::new()
        .config(config(name))
        .jobs(vec![crate::job::JobInfo::new(
            JOB,
            JOB_ATTEMPTS,
            10,
            fleet_job,
        )])
        .merge(axum::Router::new().route("/fleet/open-jobs", axum::routing::get(open_jobs_route)))
}

/// A crash stops the runs of `name`: they do not count as running.
fn note_crash(name: &str) {
    JOBS.with(|log| {
        let mut log = log.borrow_mut();
        let before = log.running.len();
        log.running.retain(|_, replica| replica != name);
        let stopped = before - log.running.len();
        log.stopped += u32::try_from(stopped).unwrap_or(u32::MAX);
    });
}

async fn jobs(sim: &mut Sim) {
    JOBS.with(|log| *log.borrow_mut() = JobsLog::default());
    let draw = Draw::new(sim.seed, "fleet-jobs");
    let names = ["a", "b"];
    let fleet = Fleet::new(
        names
            .iter()
            .map(|name| Replica::named(*name).seeded_clock(Duration::from_millis(200), 200))
            .collect(),
        jobs_app,
    );
    for name in names {
        fleet.apply(sim, &Step::Mount(name));
    }

    // About one job in three runs longer than the visibility timeout.
    let count = draw.between(3, 6);
    for id in 0..count {
        let on = names[usize::try_from(id % 2).unwrap_or(0)];
        let client = sim
            .replica(on)
            .state()
            .extension::<crate::job::JobClient>()
            .expect("fleet: the sqlite job runtime installs a JobClient");
        let ms = if draw.one_in(3) {
            draw.between(2_500, 5_000)
        } else {
            draw.between(50, 1_200)
        };
        client
            .enqueue(JOB, json!({ "id": id, "ms": ms }))
            .await
            .expect("fleet: enqueue");
    }

    let mut plan = Vec::new();
    if draw.one_in(2) {
        let at = draw.millis(200, 2_000);
        let name = draw.pick(&names);
        plan.push((at, Step::Kill(name)));
        plan.push((at + draw.millis(500, 3_000), Step::Restart(name)));
    }
    if draw.one_in(2) {
        let at = draw.millis(0, 3_000);
        let name = draw.pick(&names);
        plan.push((at, Step::LoseSession(name)));
        plan.push((at + draw.millis(200, 2_500), Step::RestoreSession(name)));
    }
    for fault in 0..2 {
        if draw.one_in(3) {
            let name = draw.pick(&names);
            let p = draw.pick(&[0.05, 0.2]);
            let step = if fault == 0 {
                Step::CommitAmbiguity(name, "autumn_jobs", p)
            } else {
                Step::MidQuery(name, "autumn_jobs", p)
            };
            plan.push((Duration::ZERO, step));
            plan.push((Duration::from_secs(6), Step::ClearFaults(name)));
        }
    }

    for _ in 0..draw.between(1, 4) {
        plan.push((
            draw.millis(0, 6_000),
            Step::Get(draw.pick(&names), "/fleet/open-jobs"),
        ));
    }
    fleet.play(sim, plan, Duration::ZERO).await;
    fleet.heal(sim);
    sim.run_for(Duration::from_secs(60)).await;

    let log = JOBS.with(|log| std::mem::take(&mut *log.borrow_mut()));
    always!(
        log.overlaps.is_empty(),
        "jobs ran on two replicas at once: {log:?}"
    );
    for id in 0..count {
        always!(
            log.finished.get(&id).copied().unwrap_or(0) >= 1,
            "job {id} never finished: {log:?}"
        );
    }
    let open = open_jobs(&fleet.substrate).await;
    always!(open == 0, "{open} job rows are not completed: {log:?}");
    sometimes!(
        log.replicas.len() == names.len(),
        "fleet-jobs-every-replica-ran-a-job"
    );
    sometimes!(
        log.started.values().any(|runs| *runs > 1),
        "fleet-jobs-a-fault-made-a-job-run-again"
    );
    sometimes!(log.stopped > 0, "fleet-jobs-a-crash-stopped-a-run");
    sometimes!(
        log.long_finished > 0,
        "fleet-jobs-a-long-job-finished-past-the-visibility-timeout"
    );
    sometimes!(log.cancelled > 0, "fleet-jobs-a-lost-claim-stopped-a-run");
}

/// Job rows that are not `completed`.
async fn open_jobs(substrate: &SqliteSubstrate) -> i64 {
    use diesel_async::RunQueryDsl as _;

    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = substrate.pool().get().await.expect("fleet: checkout");
    diesel::sql_query("SELECT COUNT(*) AS n FROM autumn_jobs WHERE status <> 'completed'")
        .get_result::<Count>(&mut *conn)
        .await
        .expect("fleet: count job rows")
        .n
}

// ── scheduler ────────────────────────────────────────────────────────────────

thread_local! {
    /// `(task, tick key, replica)` per tick run.
    static TICKS: RefCell<Vec<(String, String, String)>> = const { RefCell::new(Vec::new()) };
}

fn record_tick(
    task: &'static str,
    state: &AppState,
) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
    let key = crate::scheduler::current_tick()
        .map(|tick| tick.tick_key().to_owned())
        .unwrap_or_default();
    let replica = replica_of(state);
    tracing::info!(task, tick = %key, replica = %replica, "fleet tick");
    TICKS.with(|ticks| ticks.borrow_mut().push((task.to_owned(), key, replica)));
    Box::pin(std::future::ready(Ok(())))
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the task handler type takes the state by value"
)]
fn cron_tick(state: AppState) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
    record_tick("cron", &state)
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the task handler type takes the state by value"
)]
fn fixed_tick(state: AppState) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
    record_tick("fixed", &state)
}

/// The scheduler lease TTL. Each replica reaps leases with its own clock, so
/// the TTL must stay above the clock skew between replicas (at most 4.3 s
/// here: two 1 s offsets, two 1 s steps, and the drift).
const SCHEDULER_LEASE_TTL_SECS: u64 = 5;

fn scheduler_app(name: &str) -> TestApp {
    use crate::task::{Schedule, TaskCoordination, TaskInfo};

    let mut config = config(name);
    config.scheduler.lease_ttl_secs = SCHEDULER_LEASE_TTL_SECS;
    TestApp::new().config(config).tasks(vec![
        TaskInfo {
            name: "fleet_cron".to_owned(),
            schedule: Schedule::Cron {
                expression: "*/10 * * * * *".to_owned(),
                timezone: None,
            },
            coordination: TaskCoordination::Fleet,
            handler: cron_tick,
        },
        TaskInfo {
            name: "fleet_fixed".to_owned(),
            schedule: Schedule::FixedDelay(Duration::from_secs(7)),
            coordination: TaskCoordination::Fleet,
            handler: fixed_tick,
        },
    ])
}

async fn scheduler(sim: &mut Sim) {
    TICKS.with(|ticks| ticks.borrow_mut().clear());
    let draw = Draw::new(sim.seed, "fleet-scheduler");
    let names = ["a", "b", "c"];
    let fleet = Fleet::new(
        names
            .iter()
            .map(|name| Replica::named(*name).seeded_clock(Duration::from_secs(1), 500))
            .collect(),
        scheduler_app,
    );

    // Replicas boot up to one fixed delay apart.
    let mut plan = vec![(Duration::ZERO, Step::Mount("a"))];
    for name in ["b", "c"] {
        plan.push((draw.millis(0, 6_999), Step::Mount(name)));
    }
    let late_boot = plan.iter().map(|(at, _)| *at).max().unwrap_or_default();
    for _ in 0..draw.between(0, 2) {
        let by = i64::try_from(draw.between(0, 2_000)).unwrap_or(0) - 1_000;
        plan.push((
            late_boot + draw.millis(0, 120_000),
            Step::JumpClock(draw.pick(&names), TimeDelta::milliseconds(by)),
        ));
    }
    if draw.one_in(2) {
        let at = late_boot + draw.millis(10_000, 60_000);
        let name = draw.pick(&names);
        plan.push((at, Step::Kill(name)));
        plan.push((at + draw.millis(1_000, 20_000), Step::Restart(name)));
    }
    if draw.one_in(2) {
        let at = late_boot + draw.millis(0, 150_000);
        let name = draw.pick(&names);
        plan.push((at, Step::LoseSession(name)));
        plan.push((at + draw.millis(500, 8_000), Step::RestoreSession(name)));
    }
    if draw.one_in(3) {
        let name = draw.pick(&names);
        let at = late_boot + draw.millis(0, 60_000);
        let step = if draw.one_in(2) {
            Step::CommitAmbiguity(name, "autumn_scheduler_leases", 0.1)
        } else {
            Step::MidQuery(name, "autumn_scheduler_leases", 0.1)
        };
        plan.push((at, step));
        plan.push((at + Duration::from_secs(60), Step::ClearFaults(name)));
    }
    let steps = plan
        .iter()
        .filter(|(_, step)| matches!(step, Step::JumpClock(..)))
        .count();
    fleet
        .play(sim, plan, late_boot + Duration::from_secs(180))
        .await;

    let ticks = TICKS.with(|ticks| std::mem::take(&mut *ticks.borrow_mut()));
    let mut runs: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
    for (task, key, replica) in &ticks {
        runs.entry((task, key)).or_default().push(replica);
    }
    for ((task, key), replicas) in &runs {
        always!(
            replicas.len() == 1,
            "tick {task} {key} ran {} times, on {replicas:?}",
            replicas.len()
        );
    }
    let cron = runs.keys().filter(|(task, _)| *task == "cron").count();
    always!(
        cron >= 9,
        "only {cron} of about 18 cron ticks ran: {ticks:?}"
    );
    let leaders: BTreeSet<&str> = ticks
        .iter()
        .map(|(_, _, replica)| replica.as_str())
        .collect();
    sometimes!(
        leaders.len() == names.len(),
        "fleet-scheduler-every-replica-led-a-tick"
    );
    sometimes!(steps > 0, "fleet-scheduler-ran-through-a-clock-step");
    let lease_faults = names
        .iter()
        .flat_map(|name| sim.db_link(name).events())
        .filter(|event| event.fired() && event.table() == "autumn_scheduler_leases")
        .count();
    sometimes!(lease_faults > 0, "fleet-scheduler-a-lease-write-faulted");
}

// ── lock ─────────────────────────────────────────────────────────────────────

const LOCK_TTL: Duration = Duration::from_secs(3);

/// The shortest session loss that can lapse a lease: the holder misses two
/// renewals one third of the TTL apart. Less the clock skew and a margin.
const LAPSE_LOSS: Duration = Duration::from_millis(750);

/// One critical section: who held the lock, from when, until when.
#[derive(Debug, Clone)]
struct Hold {
    replica: &'static str,
    from: Duration,
    to: Duration,
}

thread_local! {
    static HOLDS: RefCell<Vec<Hold>> = const { RefCell::new(Vec::new()) };
    /// The replica in its critical section now, if any.
    static HOLDER: Cell<Option<&'static str>> = const { Cell::new(None) };
}

fn lock_app(name: &str) -> TestApp {
    TestApp::new().config(config(name))
}

/// Take the lock in a loop: hold it a seeded time, then pause.
fn lock_loop(
    sim: &Sim,
    name: &'static str,
    start: tokio::time::Instant,
) -> tokio::task::JoinHandle<()> {
    let state = sim.replica(name).state().clone();
    let draw = Draw::new(sim.seed, &format!("fleet-lock-{name}"));
    tokio::spawn(async move {
        let lock = crate::lock::Lock::from_state(&state, "autumn-fleet-lock")
            .expect("fleet: lock")
            .with_lease_ttl(LOCK_TTL);
        loop {
            let hold = draw.millis(100, 4_500);
            let attempt = lock
                .try_with(|| async move {
                    let from = start.elapsed();
                    tracing::info!(replica = name, "fleet lock held");
                    HOLDER.with(|holder| holder.set(Some(name)));
                    tokio::time::sleep(hold).await;
                    HOLDER.with(|holder| {
                        if holder.get() == Some(name) {
                            holder.set(None);
                        }
                    });
                    let to = start.elapsed();
                    tracing::info!(replica = name, "fleet lock released");
                    HOLDS.with(|holds| {
                        holds.borrow_mut().push(Hold {
                            replica: name,
                            from,
                            to,
                        });
                    });
                })
                .await;
            if let Err(error) = attempt {
                tracing::info!(replica = name, %error, "fleet lock attempt failed");
            }
            tokio::time::sleep(draw.millis(100, 2_000)).await;
        }
    })
}

async fn lock(sim: &mut Sim) {
    HOLDS.with(|holds| holds.borrow_mut().clear());
    HOLDER.with(|holder| holder.set(None));
    let draw = Draw::new(sim.seed, "fleet-lock");
    let names = ["a", "b", "c"];
    let fleet = Fleet::new(
        names
            .iter()
            .map(|name| Replica::named(*name).seeded_clock(Duration::from_millis(100), 100))
            .collect(),
        lock_app,
    );
    for name in names {
        fleet.apply(sim, &Step::Mount(name));
    }
    let start = tokio::time::Instant::now();
    let loops: Vec<_> = names
        .iter()
        .map(|name| lock_loop(sim, name, start))
        .collect();

    // Seeded session losses on any replica, and losses aimed at the replica
    // that holds the lock at that moment.
    let mut plan: Vec<(Duration, LockStep)> = Vec::new();
    for _ in 0..draw.between(1, 3) {
        let name = draw.pick(&names);
        let from = draw.millis(0, 50_000);
        plan.push((from, LockStep::Fleet(Step::LoseSession(name))));
        plan.push((
            from + draw.millis(300, 6_000),
            LockStep::Fleet(Step::RestoreSession(name)),
        ));
    }
    for _ in 0..draw.between(1, 2) {
        let length = if draw.one_in(2) {
            draw.millis(200, 700)
        } else {
            draw.millis(1_500, 6_000)
        };
        plan.push((draw.millis(0, 55_000), LockStep::LoseHolder(length)));
    }
    if draw.one_in(2) {
        let name = draw.pick(&names);
        plan.push((
            Duration::ZERO,
            LockStep::Fleet(Step::CommitAmbiguity(name, "autumn_locks", 0.1)),
        ));
        plan.push((
            Duration::from_secs(60),
            LockStep::Fleet(Step::ClearFaults(name)),
        ));
    }
    let mut lost_or_back: Vec<(Duration, &'static str, bool)> = Vec::new();
    let mut now = Duration::ZERO;
    while !plan.is_empty() {
        plan.sort_by_key(|(at, _)| *at);
        let (at, step) = plan.remove(0);
        if at > now {
            sim.run_for(at.saturating_sub(now)).await;
            now = at;
        }
        match step {
            LockStep::Fleet(step) => {
                match step {
                    Step::LoseSession(name) => lost_or_back.push((now, name, true)),
                    Step::RestoreSession(name) => lost_or_back.push((now, name, false)),
                    _ => {}
                }
                fleet.apply(sim, &step);
            }
            LockStep::LoseHolder(length) => {
                if let Some(holder) = HOLDER.with(Cell::get) {
                    tracing::info!(
                        replica = holder,
                        ?length,
                        "fleet: the lock holder loses its session"
                    );
                    fleet.apply(sim, &Step::LoseSession(holder));
                    lost_or_back.push((now, holder, true));
                    plan.push((now + length, LockStep::Fleet(Step::RestoreSession(holder))));
                }
            }
        }
    }
    sim.run_for(Duration::from_secs(70).saturating_sub(now))
        .await;
    for handle in loops {
        handle.abort();
    }
    HOLDER.with(|holder| holder.set(None));
    let holds = HOLDS.with(|holds| std::mem::take(&mut *holds.borrow_mut()));
    check_lock_holds(&holds, &loss_windows(&lost_or_back));
}

/// Check the lock invariant over `holds`, with the session `losses` per
/// replica.
fn check_lock_holds(holds: &[Hold], losses: &[(&'static str, Duration, Duration)]) {
    // Session loss of `replica` inside `[from, to]`.
    let loss_in = |replica: &str, from: Duration, to: Duration| -> Duration {
        losses
            .iter()
            .filter(|(name, _, _)| *name == replica)
            .map(|(_, a, b)| (*b).min(to).saturating_sub((*a).max(from)))
            .sum()
    };
    let mut lapsed = 0;
    for (i, first) in holds.iter().enumerate() {
        for second in &holds[i + 1..] {
            let (early, late) = if first.from <= second.from {
                (first, second)
            } else {
                (second, first)
            };
            if late.from >= early.to {
                continue;
            }
            let loss = loss_in(early.replica, early.from, late.from);
            always!(
                loss >= LAPSE_LOSS,
                "{} took the lock at {:?} while {} held it since {:?}, after only {loss:?} \
                 of session loss: {holds:?}",
                late.replica,
                late.from,
                early.replica,
                early.from
            );
            lapsed += 1;
        }
    }
    let holders: BTreeSet<&str> = holds.iter().map(|hold| hold.replica).collect();
    sometimes!(holders.len() > 1, "fleet-lock-passed-between-replicas");
    sometimes!(lapsed > 0, "fleet-lock-lapsed-after-a-long-session-loss");
    let short_loss_held = holds.iter().any(|hold| {
        let loss = loss_in(hold.replica, hold.from, hold.to);
        loss > Duration::ZERO && loss < LAPSE_LOSS
    });
    sometimes!(
        short_loss_held,
        "fleet-lock-held-through-a-short-session-loss"
    );
}

/// One step of the lock scenario.
#[derive(Debug, Clone)]
enum LockStep {
    Fleet(Step),
    /// Drop the session of the replica that holds the lock now, for this long.
    LoseHolder(Duration),
}

/// The session-loss windows `(replica, from, to)` from the lose and restore
/// times. A loss of a replica that already lost its session changes nothing,
/// as on the link.
fn loss_windows(
    changes: &[(Duration, &'static str, bool)],
) -> Vec<(&'static str, Duration, Duration)> {
    let mut lost_since: BTreeMap<&'static str, Duration> = BTreeMap::new();
    let mut windows = Vec::new();
    for &(at, name, lost) in changes {
        if lost {
            lost_since.entry(name).or_insert(at);
        } else if let Some(from) = lost_since.remove(name) {
            windows.push((name, from, at));
        }
    }
    windows.extend(
        lost_since
            .into_iter()
            .map(|(name, from)| (name, from, Duration::MAX)),
    );
    windows
}

// ── sweep ────────────────────────────────────────────────────────────────────

/// The result of a [`sweep`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetOutcome {
    /// Every run passed, and every `sometimes!` label was reached.
    Passed {
        /// The scenario runs, not counting the trace-check reruns.
        runs: u64,
    },
    /// A run failed. The sweep stopped there.
    Failed {
        /// The scenario.
        scenario: String,
        /// The seed.
        seed: u64,
        /// The failure message.
        reason: String,
    },
    /// A rerun of one seed logged a different trace. The sweep stopped there.
    Nondeterministic {
        /// The scenario.
        scenario: String,
        /// The seed.
        seed: u64,
        /// The first difference.
        diff: super::trace::TraceDiff,
    },
    /// Every run passed, but a `sometimes!` label was never reached.
    Vacuous {
        /// The scenario runs.
        runs: u64,
        /// The labels no run reached.
        unsatisfied: BTreeSet<String>,
    },
}

/// Run each of `scenarios` for each seed in `seeds`. Run the first
/// `trace_checks` seeds twice and compare their traces.
#[must_use]
pub fn sweep(seeds: std::ops::Range<u64>, scenarios: &[&str], trace_checks: u64) -> FleetOutcome {
    let mut observed = BTreeSet::new();
    let mut satisfied = BTreeSet::new();
    let mut runs = 0;
    for seed in seeds.clone() {
        for scenario in scenarios {
            let failed = |reason: String| FleetOutcome::Failed {
                scenario: (*scenario).to_owned(),
                seed,
                reason,
            };
            let trace = match run(scenario, seed) {
                Ok(trace) => trace,
                Err(reason) => return failed(reason),
            };
            let (seen, reached) = super::assert::sometimes_snapshot();
            observed.extend(seen);
            satisfied.extend(reached);
            runs += 1;
            if seed - seeds.start < trace_checks {
                match run(scenario, seed) {
                    Ok(again) => {
                        if let Some(diff) = trace.diff(&again) {
                            return FleetOutcome::Nondeterministic {
                                scenario: (*scenario).to_owned(),
                                seed,
                                diff,
                            };
                        }
                    }
                    Err(reason) => return failed(format!("the rerun failed: {reason}")),
                }
            }
        }
    }
    let unsatisfied: BTreeSet<String> = observed.difference(&satisfied).cloned().collect();
    if unsatisfied.is_empty() {
        FleetOutcome::Passed { runs }
    } else {
        FleetOutcome::Vacuous { runs, unsatisfied }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_sweep_reports_an_unknown_scenario_as_a_failure() {
        let outcome = super::sweep(0..2, &["nope"], 0);
        assert!(
            matches!(&outcome, super::FleetOutcome::Failed { scenario, seed: 0, .. } if scenario == "nope"),
            "{outcome:?}"
        );
    }

    #[test]
    fn loss_windows_follow_the_link_state() {
        use std::time::Duration;
        let s = Duration::from_secs;
        let windows = super::loss_windows(&[
            (s(1), "a", true),
            (s(2), "a", true),
            (s(3), "b", false),
            (s(4), "a", false),
            (s(5), "a", false),
            (s(6), "b", true),
        ]);
        assert_eq!(windows, [("a", s(1), s(4)), ("b", s(6), Duration::MAX)]);
    }

    #[test]
    fn an_unknown_scenario_is_an_error() {
        let error = super::run("nope", 0).expect_err("unknown");
        assert!(error.contains("nope") && error.contains("jobs"), "{error}");
    }
}
