//! Postgres scheduler coordination across replicas (issue #3052).
//!
//! **Requires Docker.** Each test starts its own Postgres container. Each
//! "replica" has its own pool, so it has its own database sessions.

#![cfg(all(feature = "db", not(feature = "sqlite")))]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::scheduler::{PostgresTickSchedulerCoordinator, SchedulerCoordinator};
use autumn_web::task::TaskCoordination;
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const TASK: &str = "nightly-invoice";
const PREFIX: &str = "app:scheduler";

async fn start_postgres() -> (ContainerAsync<Postgres>, String) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
    (container, url)
}

/// A pool whose sessions carry `application_name = replica`.
fn replica_pool(url: &str, replica: &str, max_size: usize) -> Pool<AsyncPgConnection> {
    let url = format!("{url}?application_name={replica}");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    Pool::builder(manager)
        .max_size(max_size)
        .build()
        .expect("pool")
}

fn coordinator(pool: Pool<AsyncPgConnection>, replica: &str) -> PostgresTickSchedulerCoordinator {
    PostgresTickSchedulerCoordinator::new(pool, replica, PREFIX)
}

/// Run one tick the way the app scheduler does: acquire, run, release.
async fn run_tick(
    coordinator: &dyn SchedulerCoordinator,
    tick_key: &str,
    runs: &AtomicUsize,
) -> bool {
    let lease = coordinator
        .try_acquire(TASK, tick_key, TaskCoordination::Fleet)
        .await
        .expect("acquire must not error");
    let Some(lease) = lease else {
        return false;
    };
    runs.fetch_add(1, Ordering::SeqCst);
    lease.release().await.expect("release must not error");
    true
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = diesel::sql_types::BigInt)]
    count: i64,
}

async fn session_count(pool: &Pool<AsyncPgConnection>, replica: &str) -> i64 {
    // Scoped here: in scope at module level, its blanket `load` would shadow
    // `AtomicUsize::load`.
    use diesel_async::RunQueryDsl as _;

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("SELECT count(*) AS count FROM pg_stat_activity WHERE application_name = $1")
        .bind::<diesel::sql_types::Text, _>(replica)
        .get_result::<CountRow>(&mut conn)
        .await
        .expect("count sessions")
        .count
}

/// Wait until the database has no session left for `replica`.
async fn wait_until_gone(observer: &Pool<AsyncPgConnection>, replica: &str) {
    for _ in 0..200 {
        if session_count(observer, replica).await == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("sessions of {replica} did not end");
}

/// AC 1: replica B's timer reaches the tick after replica A finished it.
/// The tick must run once. Before #3052 the unlock freed the key and B ran
/// the tick again.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_tick_runs_once_when_a_late_replica_reaches_it_after_the_leader_finished() {
    let (_container, url) = start_postgres().await;
    let a = coordinator(replica_pool(&url, "replica-a", 2), "replica-a");
    let b = coordinator(replica_pool(&url, "replica-b", 2), "replica-b");
    let runs = AtomicUsize::new(0);
    let tick = "nightly-invoice:1700000000";

    assert!(run_tick(&a, tick, &runs).await, "the first replica leads");
    assert!(
        !run_tick(&b, tick, &runs).await,
        "a late replica must not run a finished tick"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

/// AC 2: the leader crashes mid-tick. The documented policy is at-most-once
/// per tick: no replica runs that tick again, and the next tick runs.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_tick_is_not_rerun_after_the_leader_crashes_mid_tick() {
    let (_container, url) = start_postgres().await;
    let observer = replica_pool(&url, "observer", 1);
    let pool_a = replica_pool(&url, "replica-a", 2);
    let a = coordinator(pool_a.clone(), "replica-a");
    let b = coordinator(replica_pool(&url, "replica-b", 2), "replica-b");
    let tick = "nightly-invoice:1700000000";

    let lease = a
        .try_acquire(TASK, tick, TaskCoordination::Fleet)
        .await
        .expect("acquire")
        .expect("replica A leads the tick");
    // Crash: no release, and every session of replica A ends.
    drop(lease);
    drop(a);
    pool_a.close();
    drop(pool_a);
    wait_until_gone(&observer, "replica-a").await;

    let runs = AtomicUsize::new(0);
    assert!(
        !run_tick(&b, tick, &runs).await,
        "a crashed tick must not run again (at-most-once per tick)"
    );
    assert!(
        run_tick(&b, "nightly-invoice:1700000060", &runs).await,
        "the next tick must still run"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

/// Many replicas race for one tick at the same instant. One wins.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_tick_has_one_winner_when_replicas_race() {
    let (_container, url) = start_postgres().await;
    let runs = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for index in 0..8 {
        let replica = format!("replica-{index}");
        let coordinator = coordinator(replica_pool(&url, &replica, 1), &replica);
        let runs = Arc::clone(&runs);
        handles.push(tokio::spawn(async move {
            run_tick(&coordinator, "nightly-invoice:1700000000", &runs).await
        }));
    }
    let mut winners = 0;
    for handle in handles {
        if handle.await.expect("join") {
            winners += 1;
        }
    }
    assert_eq!(winners, 1);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

/// Each tick key is claimed on its own, and the key prefix scopes the claim.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_ticks_are_scoped_by_tick_key_and_key_prefix() {
    let (_container, url) = start_postgres().await;
    let pool = replica_pool(&url, "replica-a", 2);
    let a = coordinator(pool.clone(), "replica-a");
    let other_app = PostgresTickSchedulerCoordinator::new(pool, "replica-a", "other:scheduler");
    let runs = AtomicUsize::new(0);

    assert!(run_tick(&a, "nightly-invoice:1", &runs).await);
    assert!(run_tick(&a, "nightly-invoice:2", &runs).await);
    assert!(
        run_tick(&other_app, "nightly-invoice:1", &runs).await,
        "another key prefix is another app"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 3);
}

/// A held lease must not pin a pool connection, so a pool of one can claim a
/// second tick while the first still runs. This is also why the coordinator
/// works behind a transaction-mode `PgBouncer`: it keeps no session state.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_held_lease_does_not_pin_a_pool_connection() {
    let (_container, url) = start_postgres().await;
    let a = coordinator(replica_pool(&url, "replica-a", 1), "replica-a");

    let first = a
        .try_acquire(TASK, "nightly-invoice:1", TaskCoordination::Fleet)
        .await
        .expect("acquire")
        .expect("first tick");
    let second = tokio::time::timeout(
        Duration::from_secs(5),
        a.try_acquire(TASK, "nightly-invoice:2", TaskCoordination::Fleet),
    )
    .await
    .expect("a held lease must not block the pool")
    .expect("acquire")
    .expect("second tick");
    first.release().await.expect("release");
    second.release().await.expect("release");
}

/// The fencing token is the tick row's `generation`. It increases with each
/// claim, and a per-replica task gets none.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_fencing_token_increases_with_each_claim() {
    let (_container, url) = start_postgres().await;
    let a = coordinator(replica_pool(&url, "replica-a", 2), "replica-a");
    let b = coordinator(replica_pool(&url, "replica-b", 2), "replica-b");

    let mut tokens = Vec::new();
    for (index, replica) in [&a, &b, &a].into_iter().enumerate() {
        let lease = replica
            .try_acquire(
                TASK,
                &format!("nightly-invoice:{index}"),
                TaskCoordination::Fleet,
            )
            .await
            .expect("acquire")
            .expect("a new tick is free");
        tokens.push(lease.fencing_token().expect("a postgres tick has a token"));
        lease.release().await.expect("release");
    }
    assert!(
        tokens.windows(2).all(|pair| pair[0] < pair[1]),
        "{tokens:?}"
    );

    let local = a
        .try_acquire(TASK, "nightly-invoice:0", TaskCoordination::PerReplica)
        .await
        .expect("acquire")
        .expect("a per-replica task always runs");
    assert_eq!(local.fencing_token(), None);
}

/// A tick row stays for the retention, then the next claim of the same key
/// prefix deletes it. Rows of another key prefix stay.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn pg_tick_rows_are_pruned_after_the_retention() {
    use diesel_async::RunQueryDsl as _;

    let (_container, url) = start_postgres().await;
    let pool = replica_pool(&url, "replica-a", 2);
    let a = coordinator(pool.clone(), "replica-a").with_tick_retention(Duration::from_secs(1));
    let other_app =
        PostgresTickSchedulerCoordinator::new(pool.clone(), "replica-a", "other:scheduler");
    let runs = AtomicUsize::new(0);

    assert!(run_tick(&other_app, "nightly-invoice:1", &runs).await);
    assert!(run_tick(&a, "nightly-invoice:1", &runs).await);
    assert!(
        !run_tick(&a, "nightly-invoice:1", &runs).await,
        "inside the retention the tick stays claimed"
    );
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(run_tick(&a, "nightly-invoice:2", &runs).await);

    let mut conn = pool.get().await.expect("conn");
    let rows = diesel::sql_query(
        "SELECT count(*) AS count FROM autumn_scheduler_ticks WHERE key_prefix = $1",
    )
    .bind::<diesel::sql_types::Text, _>(PREFIX)
    .get_result::<CountRow>(&mut conn)
    .await
    .expect("count")
    .count;
    assert_eq!(rows, 1, "the expired row of tick 1 is gone");
    let other_rows = diesel::sql_query(
        "SELECT count(*) AS count FROM autumn_scheduler_ticks WHERE key_prefix = 'other:scheduler'",
    )
    .get_result::<CountRow>(&mut conn)
    .await
    .expect("count")
    .count;
    assert_eq!(other_rows, 1, "another app keeps its rows");
}
