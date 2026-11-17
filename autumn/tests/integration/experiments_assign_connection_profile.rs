//! Ledger findings harness for `ExperimentService::assign` on
//! [`PgExperimentStore`], the store an app installs to run A/B experiments
//! from Postgres.
//!
//! The experiment configuration comes from an in-memory snapshot. The other
//! store calls on the assign path (`get_override`, `get_assignment`,
//! `has_assignment_in_group`, `record_assignment`) each call
//! `Shared::connect`, which runs `diesel::PgConnection::establish` and then
//! `SET statement_timeout`. So every call opens a new Postgres session, and a
//! single `assign` opens two to four of them. This harness counts those
//! sessions (`pg_stat_database.sessions`) and the statements and buffers they
//! cost (`pg_stat_statements`, with `track_planning` on), then runs the same
//! reads on one held connection for comparison.
//!
//! It records findings and changes nothing. Fixing it means giving the store
//! a pool or a held connection. That changes how the store manages
//! connections, and it needs a maintainer's decision.
//!
//! Point `AUTUMN_LEDGER_PG_URL` at a Postgres 16 server that preloads
//! `pg_stat_statements`, or leave it unset and the harness starts a
//! testcontainer. `AUTUMN_LEDGER_ASSIGNMENTS` sets the fixture size (default
//! 200,000 rows). Reproduce with:
//!
//! ```text
//! cargo test -p autumn-web --test integration_tests -- --ignored \
//!   experiments_assign_connection_profile --nocapture --test-threads=1
//! ```
//!
//! See `docs/perf/experiment-assign/README.md` for the recorded results.
//!
//! ## Fixture
//!
//! A year of A/B testing on one app:
//!
//! - 40 experiments: 8 running (three of them share the `checkout` exclusion
//!   group), 3 concluded, 25 archived, 4 drafts.
//! - `AUTUMN_LEDGER_ASSIGNMENTS` sticky assignments. Rows fall on the 36
//!   non-draft experiments with a Zipf skew. Actors are drawn from a universe
//!   of a third as many actors, skewed towards a core of frequent visitors.
//!   Three in five are `user:<id>`, two in five are anonymous session ids. 1%
//!   are overrides. `assigned_at` covers a year. An actor holds at most one
//!   experiment in the `checkout` group.
//! - 40 QA overrides on running experiments.
//! - 3% of assignments are updated after the load, and `ANALYZE` runs with no
//!   `VACUUM`, so the planner sees dead tuples.
//!
//! ## Workload
//!
//! 1,000 `assign` calls in a fixed order: 800 returning actors on running
//! experiments, 150 new actors (spread over all eight running experiments, so
//! 56 land in the exclusion group), 20 QA-override actors, and 30 calls on
//! concluded experiments. The snapshot TTL is one hour and the snapshot is
//! loaded before the counters are reset, so no refresh runs in the window.

#![cfg(feature = "db")]
#![allow(
    clippy::cast_precision_loss,
    reason = "per-call ratios of counts far below 2^52, printed to one decimal"
)]

use std::sync::Arc;
use std::time::Duration;

use autumn_web::experiments::ExperimentService;
use autumn_web::experiments::pg::PgExperimentStore;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName, RunQueryDsl};
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const MIGRATION_SQL: &str =
    include_str!("../../migrations/20260530300000_create_experiments/up.sql");

/// The experiments half of migration `20260831191622_retention_sweep_indexes`.
/// The other two indexes there are on tables this fixture does not create.
const RETENTION_INDEX_SQL: &str = "CREATE INDEX IF NOT EXISTS idx_autumn_exp_assignments_assigned_at \
     ON autumn_experiment_assignments (assigned_at);";

/// Seeds the fixture described in the module docs. `:rows` is the number of
/// assignment rows to draw.
const FIXTURE_SQL: &str = r#"
SELECT setseed(0.4242);

INSERT INTO autumn_experiments (name, description, state, variants, winner, exclusion_group, created_at, updated_at)
SELECT format('exp_%s', lpad(i::text, 2, '0')),
       format('experiment %s', i),
       (CASE WHEN i <= 8 THEN 'running' WHEN i <= 11 THEN 'concluded'
             WHEN i <= 36 THEN 'archived' ELSE 'draft' END)::autumn_experiment_state,
       CASE WHEN i % 3 = 0
            THEN '[{"name":"control","weight":34},{"name":"a","weight":33},{"name":"b","weight":33}]'::jsonb
            ELSE '[{"name":"control","weight":50},{"name":"treatment","weight":50}]'::jsonb END,
       CASE WHEN i BETWEEN 9 AND 11 THEN 'control' END,
       CASE WHEN i BETWEEN 6 AND 8 THEN 'checkout' END,
       now() - make_interval(days => 400 - i * 9),
       now() - make_interval(days => 400 - i * 9)
FROM generate_series(1, 40) AS i;

-- Zipf over the 36 non-draft experiments: the experiment of rank r gets a
-- share proportional to 1/r. The ranks are spread over the four states.
CREATE TEMP TABLE ledger_exp_rank AS
SELECT e.name, e.three, r.rank,
       sum(1.0 / r.rank) OVER (ORDER BY r.rank) / sum(1.0 / r.rank) OVER () AS cum
FROM (SELECT name, jsonb_array_length(variants) = 3 AS three,
             row_number() OVER (ORDER BY id) AS id
      FROM autumn_experiments WHERE state <> 'draft') e
JOIN (SELECT g AS id, ((g * 7) % 36) + 1 AS rank FROM generate_series(1, 36) AS g) r USING (id);

INSERT INTO autumn_experiment_assignments (experiment, actor, variant, is_override, assigned_at)
SELECT x.name,
       CASE WHEN x.a % 5 < 3 THEN 'user:' || x.a ELSE md5('anon' || x.a) END,
       CASE WHEN x.three AND x.h % 3 = 1 THEN 'a'
            WHEN x.three AND x.h % 3 = 2 THEN 'b'
            WHEN NOT x.three AND x.h % 2 = 1 THEN 'treatment'
            ELSE 'control' END,
       x.h % 100 = 0,
       now() - make_interval(secs => (random() * 365 * 86400)::int)
FROM (
    SELECT (SELECT name FROM ledger_exp_rank WHERE cum >= u.p ORDER BY cum LIMIT 1) AS name,
           (SELECT three FROM ledger_exp_rank WHERE cum >= u.p ORDER BY cum LIMIT 1) AS three,
           1 + floor((:rows / 3) * power(u.q, 2))::bigint AS a,
           abs(hashtext(u.p::text || u.q::text)) AS h
    FROM (SELECT random() AS p, random() AS q FROM generate_series(1, :rows)) u
) x
ON CONFLICT (experiment, actor) DO NOTHING;

-- An actor holds at most one experiment of the exclusion group.
DELETE FROM autumn_experiment_assignments a
USING autumn_experiment_assignments b
WHERE a.actor = b.actor AND a.id > b.id
  AND a.experiment IN ('exp_06', 'exp_07', 'exp_08')
  AND b.experiment IN ('exp_06', 'exp_07', 'exp_08');

INSERT INTO autumn_experiment_overrides (experiment, actor, variant)
SELECT format('exp_%s', lpad((1 + i % 8)::text, 2, '0')), 'qa:' || i, 'control'
FROM generate_series(1, 40) AS i;

UPDATE autumn_experiment_assignments SET variant = variant WHERE id % 33 = 0;
ANALYZE autumn_experiments;
ANALYZE autumn_experiment_assignments;
ANALYZE autumn_experiment_overrides;
"#;

#[derive(QueryableByName)]
struct Pair {
    #[diesel(sql_type = Text)]
    experiment: String,
    #[diesel(sql_type = Text)]
    actor: String,
}

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(QueryableByName)]
struct StatementRow {
    #[diesel(sql_type = Text)]
    query: String,
    #[diesel(sql_type = BigInt)]
    calls: i64,
    #[diesel(sql_type = BigInt)]
    exec_buffers: i64,
    #[diesel(sql_type = BigInt)]
    plan_buffers: i64,
}

fn count(conn: &mut PgConnection, sql: &str) -> i64 {
    diesel::sql_query(sql)
        .get_result::<Count>(conn)
        .expect("count")
        .n
}

/// Sessions started on this database so far, and the shared buffers every
/// client backend has touched (`pg_stat_io`, hits plus reads; it counts
/// backend start-up, which `pg_stat_statements` cannot see). Other backends
/// flush these as they exit, so poll until two reads agree. This session
/// flushes its own pending counts first, so the fixture load is not charged
/// to the window that follows it.
fn counters(conn: &mut PgConnection) -> (i64, i64) {
    let mut last = (-1, -1);
    for _ in 0..50 {
        conn.batch_execute("SELECT pg_stat_force_next_flush()")
            .expect("flush own stats");
        conn.batch_execute("SELECT pg_stat_clear_snapshot()")
            .expect("clear snapshot");
        let now = (
            count(
                conn,
                "SELECT sessions AS n FROM pg_stat_database WHERE datname = current_database()",
            ),
            count(
                conn,
                "SELECT (sum(coalesce(hits, 0)) + sum(coalesce(reads, 0)))::bigint AS n \
                 FROM pg_stat_io WHERE backend_type = 'client backend'",
            ),
        );
        if now == last {
            return now;
        }
        last = now;
        std::thread::sleep(Duration::from_millis(300));
    }
    last
}

fn reset(conn: &mut PgConnection) {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .expect("reset pg_stat_statements");
}

/// Print the statements since the last reset, and return the totals. On
/// Postgres 16 with `track_planning` on, `shared_blks_*` include the
/// planner's buffers; Postgres 17 splits them out, and this reads only the
/// execution half there.
///
/// Totals:
/// (statements, `SET statement_timeout` calls, execution buffers, planning
/// buffers).
fn profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64) {
    std::thread::sleep(Duration::from_millis(500));
    let rows = diesel::sql_query(
        "SELECT query, calls, \
                (shared_blks_hit + shared_blks_read)::bigint AS exec_buffers, \
                0::bigint AS plan_buffers \
         FROM pg_stat_statements \
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
           AND query NOT ILIKE '%pg_stat%' \
         ORDER BY calls DESC, query",
    )
    .load::<StatementRow>(conn)
    .expect("read pg_stat_statements");
    println!("\n=== pg_stat_statements: {label} ===");
    println!(
        "{:>6} {:>10} {:>10}  statement",
        "calls", "exec_buf", "plan_buf"
    );
    let (mut calls, mut sets, mut exec, mut plan) = (0, 0, 0, 0);
    for row in &rows {
        let text: String = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        let short: String = text.chars().take(110).collect();
        println!(
            "{:>6} {:>10} {:>10}  {short}",
            row.calls, row.exec_buffers, row.plan_buffers
        );
        calls += row.calls;
        exec += row.exec_buffers;
        plan += row.plan_buffers;
        if text.starts_with("SET statement_timeout") {
            sets += row.calls;
        }
    }
    println!(
        "-- total: statements={calls} (SET statement_timeout={sets}) \
         exec_buffers={exec} plan_buffers={plan} buffers={} --",
        exec + plan
    );
    (calls, sets, exec, plan)
}

async fn server() -> (String, Option<testcontainers::ContainerAsync<Postgres>>) {
    if let Ok(url) = std::env::var("AUTUMN_LEDGER_PG_URL") {
        return (url, None);
    }
    let container = Postgres::default()
        .with_tag("16-alpine")
        .with_cmd([
            "-c",
            "fsync=off",
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-c",
            "pg_stat_statements.track=all",
            "-c",
            "pg_stat_statements.track_planning=on",
        ])
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    (
        format!("postgres://postgres:postgres@{host}:{port}/postgres"),
        Some(container),
    )
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn experiments_assign_connection_profile() {
    let (url, _container) = server().await;
    let n: i64 = std::env::var("AUTUMN_LEDGER_ASSIGNMENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);

    let mut admin = PgConnection::establish(&url).expect("connect");
    admin
        .batch_execute(
            "CREATE EXTENSION IF NOT EXISTS pg_stat_statements; \
             DROP TABLE IF EXISTS autumn_experiment_changes, autumn_experiment_overrides, \
               autumn_experiment_assignments, autumn_experiments CASCADE; \
             DROP TYPE IF EXISTS autumn_experiment_state CASCADE;",
        )
        .expect("reset schema");
    admin.batch_execute(MIGRATION_SQL).expect("migration");
    admin
        .batch_execute(RETENTION_INDEX_SQL)
        .expect("retention index");
    admin
        .batch_execute(&FIXTURE_SQL.replace(":rows", &n.to_string()))
        .expect("fixture");

    let rows = count(
        &mut admin,
        "SELECT count(*) AS n FROM autumn_experiment_assignments",
    );
    let actors = count(
        &mut admin,
        "SELECT count(DISTINCT actor) AS n FROM autumn_experiment_assignments",
    );
    let dead = count(
        &mut admin,
        "SELECT n_dead_tup AS n FROM pg_stat_user_tables \
         WHERE relname = 'autumn_experiment_assignments'",
    );
    println!("fixture: assignments={rows} distinct_actors={actors} dead_tuples={dead}");

    let returning = diesel::sql_query(
        "SELECT a.experiment, a.actor FROM autumn_experiment_assignments a \
         JOIN autumn_experiments e ON e.name = a.experiment \
         WHERE e.state = 'running' AND a.actor NOT LIKE 'qa:%' \
         ORDER BY md5(a.experiment || a.actor) LIMIT 800",
    )
    .load::<Pair>(&mut admin)
    .expect("returning actors");
    assert_eq!(returning.len(), 800, "the fixture has too few assignments");
    let overrides = diesel::sql_query(
        "SELECT experiment, actor FROM autumn_experiment_overrides ORDER BY id LIMIT 20",
    )
    .load::<Pair>(&mut admin)
    .expect("override actors");

    let mut groups: Vec<(&str, Vec<(String, String)>)> = vec![
        (
            "returning actors",
            returning
                .iter()
                .map(|p| (p.experiment.clone(), p.actor.clone()))
                .collect(),
        ),
        (
            "new actors",
            (0..150)
                .map(|i| (format!("exp_0{}", 1 + i % 8), format!("user:new-{i}")))
                .collect(),
        ),
        (
            "QA overrides",
            overrides
                .iter()
                .map(|p| (p.experiment.clone(), p.actor.clone()))
                .collect(),
        ),
        (
            "concluded experiments",
            (0..30)
                .map(|i| (format!("exp_{:02}", 9 + i % 3), format!("user:{}", i + 1)))
                .collect(),
        ),
    ];

    let store = PgExperimentStore::with_cache_ttl(&url, Duration::from_secs(3_600));
    store.refresh().expect("load the experiment snapshot");
    let service = ExperimentService::new(Arc::new(store));

    // Phase 1: the real entry point, one group of calls at a time.
    let (
        mut all_calls,
        mut all_ok,
        mut all_opened,
        mut all_statements,
        mut all_buffers,
        mut all_io,
    ) = (0, 0, 0, 0, 0, 0);
    for (label, calls) in &mut groups {
        let before = counters(&mut admin);
        reset(&mut admin);
        let mut outcomes = Vec::with_capacity(calls.len());
        for (experiment, actor) in calls.iter() {
            let svc = service.clone();
            let (e, a) = (experiment.clone(), actor.clone());
            let outcome = tokio::task::spawn_blocking(move || svc.assign(&e, &a))
                .await
                .expect("assign task");
            outcomes.push(outcome.map_err(|e| e.to_string()));
        }
        let (statements, _, exec, plan) =
            profile(&mut admin, &format!("ExperimentService::assign, {label}"));
        let after = counters(&mut admin);
        let (opened, io) = (after.0 - before.0, after.1 - before.1);
        let ok = outcomes.iter().filter(|o| o.is_ok()).count();
        let excluded = outcomes
            .iter()
            .filter(|o| o.as_ref().is_err_and(|e| e.contains("excluded")))
            .count();
        assert_eq!(
            ok + excluded,
            calls.len(),
            "unexpected assign error: {outcomes:?}"
        );
        let n = calls.len() as f64;
        println!(
            "assign[{label}]: calls={} ok={ok} excluded={excluded} sessions_opened={opened} \
             sessions/call={:.2} statements/call={:.2} stmt_buffers/call={:.1} \
             backend_io_buffers/call={:.1}",
            calls.len(),
            opened as f64 / n,
            statements as f64 / n,
            (exec + plan) as f64 / n,
            io as f64 / n,
        );
        all_calls += calls.len();
        all_ok += ok;
        all_opened += opened;
        all_statements += statements;
        all_buffers += exec + plan;
        all_io += io;
    }
    let n = all_calls as f64;
    println!(
        "assign[all]: calls={all_calls} ok={all_ok} sessions_opened={all_opened} \
         sessions/call={:.2} statements/call={:.2} stmt_buffers/call={:.1} \
         backend_io_buffers/call={:.1}",
        all_opened as f64 / n,
        all_statements as f64 / n,
        all_buffers as f64 / n,
        all_io as f64 / n,
    );

    // Phase 2: the two reads a returning actor costs, on one held connection.
    // Same statement text and parameters as `get_override` and
    // `get_assignment`.
    let before = counters(&mut admin);
    reset(&mut admin);
    let mut held = PgConnection::establish(&url).expect("connect");
    held.batch_execute("SET statement_timeout = 5000")
        .expect("timeout");
    for pair in &returning {
        diesel::sql_query(
            "SELECT variant FROM autumn_experiment_overrides \
             WHERE experiment = $1 AND actor = $2",
        )
        .bind::<Text, _>(&pair.experiment)
        .bind::<Text, _>(&pair.actor)
        .execute(&mut held)
        .expect("override read");
        diesel::sql_query(
            "SELECT experiment, actor, variant, is_override, \
                    EXTRACT(EPOCH FROM assigned_at)::bigint AS assigned_at_secs \
             FROM autumn_experiment_assignments \
             WHERE experiment = $1 AND actor = $2",
        )
        .bind::<Text, _>(&pair.experiment)
        .bind::<Text, _>(&pair.actor)
        .execute(&mut held)
        .expect("assignment read");
    }
    drop(held);
    let (held_statements, _, held_exec, held_plan) =
        profile(&mut admin, "returning-actor reads on one held connection");
    let after = counters(&mut admin);
    let (held_opened, held_io) = (after.0 - before.0, after.1 - before.1);
    println!(
        "held: calls={} sessions_opened={held_opened} statements/call={:.2} buffers/call={:.1} \
         backend_io_buffers={held_io} backend_io_buffers/call={:.1}",
        returning.len(),
        held_statements as f64 / returning.len() as f64,
        (held_exec + held_plan) as f64 / returning.len() as f64,
        held_io as f64 / returning.len() as f64,
    );

    let written = count(
        &mut admin,
        "SELECT count(*) AS n FROM autumn_experiment_assignments WHERE actor LIKE 'user:new-%'",
    );
    println!("new assignments written={written}");
}
