//! Multi-replica simulation (issue #3067).
//!
//! Two or three apps share one sim clock and one `SQLite` database. Each test
//! drives the framework's own jobs, scheduler or lock across the replicas.
//!
//! Standalone `[[test]]` binary: the consolidated binary is Postgres-typed.
//! Run it with
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sim_replicas`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use autumn_web::config::{AutumnConfig, SchedulerBackend};
use autumn_web::job::JobClient;
use autumn_web::lock::Lock;
use autumn_web::prelude::*;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::sim::substrate::SqliteSubstrate;
use autumn_web::sim::{DbFaultKind, Replica, Sim};
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::time::Clock;
use chrono::TimeDelta;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use serde::{Deserialize, Serialize};
use serde_json::json;

const SECOND: Duration = Duration::from_secs(1);

type SqlitePool = Pool<autumn_web::db::RuntimeConnection>;

/// The replica id a handler runs on, from the app config.
fn replica_of(state: &AppState) -> String {
    state
        .extension::<AutumnConfig>()
        .and_then(|config| config.scheduler.replica_id.clone())
        .unwrap_or_default()
}

/// Config for one replica: `SQLite` jobs and scheduler, with short timeouts.
fn replica_config(name: &str) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    "sqlite".clone_into(&mut config.jobs.backend);
    config.jobs.workers = 1;
    config.jobs.max_attempts = 5;
    config.jobs.initial_backoff_ms = 1;
    config.jobs.sqlite.visibility_timeout_ms = 2_000;
    config.jobs.sqlite.poll_interval_ms = 100;
    config.scheduler.backend = SchedulerBackend::Sqlite;
    config.scheduler.lease_ttl_secs = 1;
    config.scheduler.replica_id = Some(name.to_owned());
    config
}

#[get("/now")]
async fn now_route(clock: Clock) -> String {
    clock.now().to_rfc3339()
}

async fn now_of(sim: &Sim, replica: &str) -> chrono::DateTime<chrono::Utc> {
    let body = sim.replica(replica).get("/now").send().await.text();
    chrono::DateTime::parse_from_rfc3339(&body)
        .expect("rfc3339")
        .with_timezone(&chrono::Utc)
}

// ── Clocks ───────────────────────────────────────────────────────────────────

#[sim_test]
async fn sim_replicas_share_one_clock_with_their_own_offsets(mut sim: Sim) {
    sim.mount_replica("a", TestApp::new().routes(routes![now_route]));
    sim.mount_replica(
        Replica::named("b").clock_behind(Duration::from_secs(3)),
        TestApp::new().routes(routes![now_route]),
    );
    sim.mount_replica(
        Replica::named("c").clock_drift_ppm(100_000),
        TestApp::new().routes(routes![now_route]),
    );
    assert_eq!(sim.replica_names(), vec!["a", "b", "c"]);

    let a0 = now_of(&sim, "a").await;
    assert_eq!(a0 - now_of(&sim, "b").await, TimeDelta::seconds(3));
    assert_eq!(now_of(&sim, "c").await, a0, "drift starts at zero");

    sim.run_for(Duration::from_secs(10)).await;

    assert_eq!(now_of(&sim, "a").await - a0, TimeDelta::seconds(10));
    assert_eq!(
        now_of(&sim, "b").await - a0,
        TimeDelta::seconds(7),
        "b keeps its offset"
    );
    assert_eq!(
        now_of(&sim, "c").await - a0,
        TimeDelta::seconds(11),
        "c runs 10% fast"
    );
}

#[sim_test]
async fn sim_replica_clock_step_moves_one_replica_only(mut sim: Sim) {
    sim.mount_replica("a", TestApp::new().routes(routes![now_route]));
    sim.mount_replica("b", TestApp::new().routes(routes![now_route]));
    sim.step_replica_clock("b", TimeDelta::seconds(5));
    assert_eq!(
        now_of(&sim, "b").await - now_of(&sim, "a").await,
        TimeDelta::seconds(5)
    );
    assert_eq!(
        sim.replica_clock("b").expect("b is mounted").offset(),
        TimeDelta::seconds(5)
    );
}

/// Replica names are network host names, so two names that only differ in
/// case are one replica name.
#[test]
#[should_panic(expected = "already mounted")]
fn sim_replica_names_that_differ_only_in_case_clash() {
    let runtime = autumn_web::sim::runtime().expect("sim runtime");
    runtime.block_on(async {
        let mut sim = Sim::from_seed(1);
        sim.mount_replica("Payments", TestApp::new());
        sim.mount_replica("payments", TestApp::new());
    });
}

#[test]
fn sim_seeded_replica_clocks_replay_from_the_seed_and_stay_in_bounds() {
    let draw = |seed: u64| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async move {
            let mut sim = Sim::from_seed(seed);
            let spec = Replica::named("a").seeded_clock(SECOND, 500);
            sim.mount_replica(spec, TestApp::new());
            sim.replica_clock("a").expect("mounted")
        })
    };
    assert_eq!(draw(7), draw(7), "same seed, same clock");
    let mut seen = std::collections::BTreeSet::new();
    for seed in 0..32 {
        let clock = draw(seed);
        assert!(clock.offset().abs() <= TimeDelta::seconds(1), "{clock:?}");
        assert!(clock.drift_ppm().abs() <= 500, "{clock:?}");
        seen.insert(clock.offset().num_milliseconds());
    }
    assert!(seen.len() > 16, "offsets vary by seed: {seen:?}");
}

// ── Jobs (#3051) ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Nothing {}

static LONG_JOB_RUNS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Runs three times longer than the 2 s visibility timeout.
#[job(name = "sim_replicas_long_job", max_attempts = 5, backoff_ms = 1)]
async fn long_job(state: AppState, _args: Nothing) -> AutumnResult<()> {
    LONG_JOB_RUNS.lock().unwrap().push(replica_of(&state));
    tokio::time::sleep(Duration::from_secs(6)).await;
    Ok(())
}

async fn enqueue_on(sim: &Sim, replica: &str, job: &str) {
    let client = sim
        .replica(replica)
        .state()
        .extension::<JobClient>()
        .expect("the sqlite job runtime installs a JobClient");
    client.enqueue(job, json!({})).await.expect("enqueue");
}

/// #3051: a job longer than the visibility timeout runs once across two
/// replicas. Before the heartbeat (#3135), the claim was not renewed, and the
/// peer ran the job again.
#[sim_test]
async fn sim_long_job_runs_once_across_two_replicas(mut sim: Sim) {
    let substrate = SqliteSubstrate::new().expect("substrate");
    for name in ["a", "b"] {
        let pool = substrate
            .replica_pool(&sim.db_link(name))
            .expect("replica pool");
        sim.mount_replica(
            name,
            TestApp::new()
                .config(replica_config(name))
                .with_db(pool)
                .jobs(jobs![long_job]),
        );
    }
    enqueue_on(&sim, "a", "sim_replicas_long_job").await;
    sim.run_for(Duration::from_secs(20)).await;

    let runs = LONG_JOB_RUNS.lock().unwrap().clone();
    assert_eq!(
        runs.len(),
        1,
        "a 6 s job with a 2 s visibility timeout ran {} times: {runs:?}",
        runs.len()
    );
}

static CRASH_JOB: Mutex<Vec<(String, &'static str)>> = Mutex::new(Vec::new());

/// Runs 1.5 s, inside the 2 s visibility timeout.
#[job(name = "sim_replicas_crash_job", max_attempts = 5, backoff_ms = 1)]
async fn crash_job(state: AppState, _args: Nothing) -> AutumnResult<()> {
    let replica = replica_of(&state);
    CRASH_JOB.lock().unwrap().push((replica.clone(), "start"));
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    CRASH_JOB.lock().unwrap().push((replica, "end"));
    Ok(())
}

/// A killed replica stops its running job, and its peer recovers the job after
/// the visibility timeout.
#[sim_test]
async fn sim_killed_replica_job_is_recovered_by_its_peer(mut sim: Sim) {
    let substrate = SqliteSubstrate::new().expect("substrate");
    let app = |name: &str, sim: &Sim| {
        TestApp::new()
            .config(replica_config(name))
            .with_db(
                substrate
                    .replica_pool(&sim.db_link(name))
                    .expect("replica pool"),
            )
            .jobs(jobs![crash_job])
    };
    let a = app("a", &sim);
    sim.mount_replica("a", a);
    enqueue_on(&sim, "a", "sim_replicas_crash_job").await;
    sim.run_for(Duration::from_millis(500)).await;
    assert_eq!(
        *CRASH_JOB.lock().unwrap(),
        vec![("a".to_owned(), "start")],
        "a claims the job first"
    );

    let b = app("b", &sim);
    sim.mount_replica("b", b);
    sim.kill_replica("a");
    sim.run_for(Duration::from_secs(10)).await;

    assert_eq!(
        *CRASH_JOB.lock().unwrap(),
        vec![
            ("a".to_owned(), "start"),
            ("b".to_owned(), "start"),
            ("b".to_owned(), "end"),
        ],
        "the kill stops a's run; b runs the job to the end"
    );
}

// ── Scheduler (#3052) ────────────────────────────────────────────────────────

static CRON_TICKS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

#[scheduled(cron = "0 * * * * *", name = "sim_replicas_cron")]
async fn cron_task(state: AppState) -> AutumnResult<()> {
    let tick = autumn_web::scheduler::current_tick()
        .map(|tick| tick.tick_key().to_owned())
        .unwrap_or_default();
    CRON_TICKS.lock().unwrap().push((replica_of(&state), tick));
    Ok(())
}

/// Whether a replica found the tick `key` already claimed.
fn claimed_already(trace: &autumn_web::sim::Trace, key: &str) -> bool {
    trace
        .lines()
        .iter()
        .any(|line| line.contains("already claimed") && line.contains(&format!("tick={key}")))
}

fn duplicates(ticks: &[(String, String)]) -> BTreeMap<String, Vec<String>> {
    let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (replica, key) in ticks {
        by_key.entry(key.clone()).or_default().push(replica.clone());
    }
    by_key.retain(|_, runs| runs.len() > 1);
    by_key
}

/// #3052: a replica whose clock steps forward wakes past the tick, after the
/// leader ran it. Before the fix the lease lasted only the 1 s TTL, so the late
/// replica took the free key and ran the tick again.
#[sim_test]
async fn sim_cron_tick_runs_once_when_a_replica_clock_steps_forward(mut sim: Sim) {
    let substrate = SqliteSubstrate::new().expect("substrate");
    for spec in [
        Replica::named("a"),
        Replica::named("b").clock_behind(Duration::from_secs(2)),
    ] {
        let name = spec.name().to_owned();
        let pool = substrate
            .replica_pool(&sim.db_link(&name))
            .expect("replica pool");
        sim.mount_replica(
            spec,
            TestApp::new()
                .config(replica_config(&name))
                .with_db(pool)
                .tasks(tasks![cron_task]),
        );
    }
    // b's timer for minute 1 is set. An NTP step then moves b 5 s ahead, so b
    // wakes 3 s after a ran the tick.
    let ((), trace) = autumn_web::sim::trace::capture(async {
        sim.run_for(Duration::from_secs(30)).await;
        sim.step_replica_clock("b", TimeDelta::seconds(5));
        sim.run_for(Duration::from_secs(150)).await;
    })
    .await;

    let ticks = CRON_TICKS.lock().unwrap().clone();
    // b starts 2 s behind, so it also runs minute 0. Then minutes 1 to 3.
    assert_eq!(ticks.len(), 4, "four distinct minutes: {ticks:?}");
    assert!(
        ticks.contains(&("a".to_owned(), "sim_replicas_cron:1577836860".to_owned())),
        "a ran the contested minute 1: {ticks:?}"
    );
    assert!(
        claimed_already(&trace, "sim_replicas_cron:1577836860"),
        "b tried the contested tick and found it claimed"
    );
    assert!(
        duplicates(&ticks).is_empty(),
        "a cron tick ran on more than one replica: {:?}",
        duplicates(&ticks)
    );
}

static FIXED_TICKS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

#[scheduled(every = "7s", name = "sim_replicas_fixed")]
async fn fixed_task(state: AppState) -> AutumnResult<()> {
    let tick = autumn_web::scheduler::current_tick()
        .map(|tick| tick.tick_key().to_owned())
        .unwrap_or_default();
    FIXED_TICKS.lock().unwrap().push((replica_of(&state), tick));
    Ok(())
}

/// #3052: replica timers start at boot, so two replicas reach one fixed-delay
/// bucket up to one delay apart. Before the fix the 1 s lease freed the bucket
/// first, and the later replica ran it again.
#[sim_test]
async fn sim_fixed_delay_tick_runs_once_across_staggered_replicas(mut sim: Sim) {
    let substrate = SqliteSubstrate::new().expect("substrate");
    let app = |name: &str, sim: &Sim| {
        TestApp::new()
            .config(replica_config(name))
            .with_db(
                substrate
                    .replica_pool(&sim.db_link(name))
                    .expect("replica pool"),
            )
            .tasks(tasks![fixed_task])
    };
    let a = app("a", &sim);
    sim.mount_replica("a", a);
    sim.run_for(Duration::from_secs(3)).await;
    let b = app("b", &sim);
    sim.mount_replica("b", b);
    let ((), trace) = autumn_web::sim::trace::capture(sim.run_for(Duration::from_secs(60))).await;

    let ticks = FIXED_TICKS.lock().unwrap().clone();
    assert!(ticks.len() >= 5, "the task ran: {ticks:?}");
    assert!(
        claimed_already(&trace, "sim_replicas_fixed:225405258"),
        "b reached a's first bucket and found it claimed"
    );
    assert!(
        duplicates(&ticks).is_empty(),
        "a fixed-delay bucket ran on more than one replica: {:?}",
        duplicates(&ticks)
    );
}

// ── Lock under DB session loss ───────────────────────────────────────────────

/// Hold the lock on `replica` for `hold`, and log the window.
async fn hold_lock(
    sim: &Sim,
    replica: &str,
    hold: Duration,
    log: &'static Mutex<Vec<(String, &'static str)>>,
) {
    let state = sim.replica(replica).state().clone();
    let lock = Lock::from_state(&state, "sim-replicas-lock")
        .expect("lock")
        .with_lease_ttl(Duration::from_secs(3));
    let name = replica.to_owned();
    let handle = tokio::spawn(async move {
        lock.try_with(|| async {
            log.lock().unwrap().push((name.clone(), "in"));
            tokio::time::sleep(hold).await;
            log.lock().unwrap().push((name, "out"));
        })
        .await
    });
    tokio::task::yield_now().await;
    drop(handle);
}

fn overlaps(log: &[(String, &'static str)]) -> bool {
    let mut inside = 0_i32;
    for (_, edge) in log {
        inside += if *edge == "in" { 1 } else { -1 };
        if inside > 1 {
            return true;
        }
    }
    false
}

fn lock_replicas(sim: &mut Sim, substrate: &SqliteSubstrate) {
    for name in ["a", "b"] {
        let pool = substrate
            .replica_pool(&sim.db_link(name))
            .expect("replica pool");
        sim.mount_replica(
            name,
            TestApp::new().config(replica_config(name)).with_db(pool),
        );
    }
}

static SHORT_LOSS: Mutex<Vec<(String, &'static str)>> = Mutex::new(Vec::new());

/// A session loss shorter than the lease: renewal retries, and the peer never
/// gets in while the holder runs.
#[sim_test]
async fn sim_lock_holds_through_a_short_db_session_loss(mut sim: Sim) {
    let substrate = SqliteSubstrate::new().expect("substrate");
    lock_replicas(&mut sim, &substrate);
    hold_lock(&sim, "a", Duration::from_secs(8), &SHORT_LOSS).await;
    sim.run_for(SECOND).await;

    sim.db_link("a").lose_session();
    sim.run_for(Duration::from_millis(1_500)).await;
    sim.db_link("a").restore_session();

    for _ in 0..8 {
        hold_lock(&sim, "b", SECOND, &SHORT_LOSS).await;
        sim.run_for(SECOND).await;
    }
    sim.run_for(Duration::from_secs(5)).await;

    let log = SHORT_LOSS.lock().unwrap().clone();
    assert!(!overlaps(&log), "two holders at once: {log:?}");
    assert!(
        log.iter().any(|(who, _)| who == "b"),
        "b gets the lock after a: {log:?}"
    );
    let refused = sim
        .db_link("a")
        .events()
        .iter()
        .filter(|event| event.kind() == DbFaultKind::SessionLost && event.fired())
        .count();
    assert!(refused > 0, "a's renewal saw the lost session");
}

static LONG_LOSS: Mutex<Vec<(String, &'static str)>> = Mutex::new(Vec::new());

/// A session loss longer than the lease: the `SQLite` lock is a lease with no
/// loss signal, so the peer gets in while the old holder still runs. The
/// harness must find that overlap (see the `Lock` docs: not for correctness).
#[sim_test]
async fn sim_lock_session_loss_past_the_lease_lets_the_peer_in(mut sim: Sim) {
    let substrate = SqliteSubstrate::new().expect("substrate");
    lock_replicas(&mut sim, &substrate);
    hold_lock(&sim, "a", Duration::from_secs(10), &LONG_LOSS).await;
    sim.run_for(SECOND).await;

    sim.db_link("a").lose_session();
    for _ in 0..6 {
        sim.run_for(SECOND).await;
        hold_lock(&sim, "b", SECOND, &LONG_LOSS).await;
    }
    sim.db_link("a").restore_session();
    sim.run_for(Duration::from_secs(10)).await;

    let log = LONG_LOSS.lock().unwrap().clone();
    assert!(overlaps(&log), "the harness finds the overlap: {log:?}");
}

// ── DB fault lanes ───────────────────────────────────────────────────────────

#[derive(diesel::QueryableByName, Debug, PartialEq)]
struct Row {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    id: i64,
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    v: i64,
}

async fn rows(pool: &SqlitePool) -> Vec<Row> {
    let mut conn = pool.get().await.expect("checkout");
    diesel::sql_query("SELECT id, v FROM fault_rows ORDER BY id")
        .load::<Row>(&mut conn)
        .await
        .expect("select")
}

async fn write(pool: &SqlitePool, sql: &str) -> Result<usize, String> {
    let mut conn = pool.get().await.map_err(|error| error.to_string())?;
    diesel::sql_query(sql)
        .execute(&mut conn)
        .await
        .map_err(|error| error.to_string())
}

#[sim_test]
async fn sim_db_link_faults_hit_one_replica_only(sim: Sim) {
    let substrate = SqliteSubstrate::new().expect("substrate");
    let a = substrate.replica_pool(&sim.db_link("a")).expect("a pool");
    let b = substrate.replica_pool(&sim.db_link("b")).expect("b pool");
    write(
        &b,
        "CREATE TABLE fault_rows (id INTEGER PRIMARY KEY, v BIGINT NOT NULL)",
    )
    .await
    .expect("create");
    write(&b, "INSERT INTO fault_rows VALUES (1, 0)")
        .await
        .expect("seed");

    // Mid-query: the write fails and does not apply.
    sim.db_link("a").mid_query_errors("fault_rows", 1.0);
    let error = write(&a, "UPDATE fault_rows SET v = 1 WHERE id = 1")
        .await
        .expect_err("mid-query fault");
    assert!(error.contains("mid-query"), "{error}");
    assert_eq!(rows(&b).await, vec![Row { id: 1, v: 0 }]);
    write(&b, "UPDATE fault_rows SET v = 2 WHERE id = 1")
        .await
        .expect("b is not faulted");

    // Commit ambiguity: the write applies, but the caller sees an error.
    sim.db_link("a").clear_faults();
    sim.db_link("a").commit_ambiguity("fault_rows", 1.0);
    let error = write(&a, "UPDATE fault_rows SET v = 3 WHERE id = 1")
        .await
        .expect_err("ambiguous commit");
    assert!(error.contains("commit outcome unknown"), "{error}");
    assert_eq!(
        rows(&b).await,
        vec![Row { id: 1, v: 3 }],
        "the write applied"
    );

    // Session loss: no checkout until the session comes back.
    sim.db_link("a").clear_faults();
    sim.db_link("a").lose_session();
    assert!(a.get().await.is_err(), "a lost its session");
    assert!(b.get().await.is_ok(), "b did not");
    sim.db_link("a").restore_session();
    assert_eq!(rows(&a).await, vec![Row { id: 1, v: 3 }]);

    let kinds: Vec<DbFaultKind> = sim
        .db_link("a")
        .events()
        .iter()
        .filter(|event| event.fired())
        .map(autumn_web::sim::DbFaultEvent::kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            DbFaultKind::MidQuery,
            DbFaultKind::CommitAmbiguous,
            DbFaultKind::SessionLost,
        ]
    );
    assert!(sim.db_link("b").events().iter().all(|event| !event.fired()));
}

// ── SimNet between replicas ──────────────────────────────────────────────────

#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

#[get("/call-b")]
async fn call_b(client: autumn_web::http_client::Client) -> String {
    match client.get("http://b/ping").send().await {
        Ok(response) => format!("ok: {}", response.text()),
        Err(error) => format!("error: {error}"),
    }
}

#[sim_test]
async fn sim_replicas_reach_each_other_over_sim_net(mut sim: Sim) {
    sim.net(autumn_web::sim::SimNet::new());
    sim.mount_replica("a", TestApp::new().routes(routes![call_b]));
    sim.mount_replica("b", TestApp::new().routes(routes![ping]));
    let body = sim.replica("a").get("/call-b").send().await.text();
    assert_eq!(body, "ok: pong");

    sim.kill_replica("b");
    let body = sim.replica("a").get("/call-b").send().await.text();
    assert!(
        body.starts_with("error:"),
        "a killed replica is unreachable: {body}"
    );
}
