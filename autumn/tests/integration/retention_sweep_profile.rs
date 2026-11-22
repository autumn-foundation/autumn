//! Ledger harness for the framework retention sweep on Postgres
//! (`sweep_postgres` in `autumn/src/data_retention.rs`).
//!
//! It calls `run_retention`, the entry point the scheduled
//! `autumn-retention-sweep` task and `autumn retention run` both use, once
//! per sweep-enforced dataset, against framework tables shaped like
//! production ones. It prints the `pg_stat_statements` profile of each
//! dataset's pass (statements, rows, shared buffers, temp blocks and WAL
//! bytes) and a fingerprint of the rows the pass left, so two builds can be
//! compared row for row.
//!
//! Point `AUTUMN_LEDGER_PG_URL` at a Postgres 16 server that preloads
//! `pg_stat_statements`, or leave it unset and the harness starts a
//! testcontainer. `AUTUMN_LEDGER_RETENTION_JOBS` sets the `autumn_jobs` size
//! (default 1,000,000 rows); the other tables scale from it. Reproduce with:
//!
//! ```text
//! cargo test -p autumn-web --test integration_tests -- --ignored \
//!   retention_sweep_profile --nocapture --test-threads=1
//! ```
//!
//! See `docs/perf/retention-sweep/README.md` for the recorded results.
//!
//! ## Fixture
//!
//! Every table is filled in time order, oldest first, as the app writes it.
//! Every timestamp is a fixed offset from the fill's `now()`, and no row is
//! within an hour of a window's cutoff, so the set a pass deletes does not
//! depend on how long the fill took.
//!
//! - `autumn_jobs`, `J` rows (`AUTUMN_LEDGER_RETENTION_JOBS`) over the last
//!   37 days, from every shipped job migration. 95.9% completed, 3% failed,
//!   1% discarded; the newest 0.1% enqueued or running. 5% carry a TTL
//!   unique key, which the sweep keeps. Window 30 days.
//! - `autumn_repository_commit_hooks`, `J / 4` rows over 37 days: 90%
//!   completed, 4% failed, 2% `after_hook_failed`, the rest enqueued,
//!   running, `pending_after_hook` or `after_hook_succeeded` (kept). Window
//!   30 days.
//! - `autumn_job_tracking`, `J / 4` rows updated over the last 9 days.
//!   Window 7 days.
//! - `autumn_experiment_assignments`, `J` rows over the last 400 days across
//!   40 experiments: 24 archived, 6 running, 5 concluded, 4 draft, and one
//!   deleted. Running, concluded and draft experiments hold 45% of the rows,
//!   which the sweep keeps however old. Window 90 days.
//! - 4% of the rows of each table are updated before `ANALYZE` with no
//!   `VACUUM`, so dead tuples are on the pages.
//!
//! ## Workload
//!
//! One `run_retention` call per dataset, in the order the scheduled task
//! runs them, each after a `CHECKPOINT` and a `pg_stat_statements` reset.
//! A pass deletes at most 1,000 batches of 500 rows, so the largest fixture
//! leaves `job_history` truncated, as a first sweep of a long-unswept table
//! does.

#![cfg(all(feature = "db", feature = "postgres"))]

use std::time::Duration;

use autumn_web::AppState;
use autumn_web::config::AutumnConfig;
use autumn_web::data_retention::{RetentionRunOptions, run_retention};
use diesel::QueryableByName;
use diesel::sql_types::{BigInt, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl as _, SimpleAsyncConnection as _};
use testcontainers::ImageExt as _;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const MIGRATIONS: &[&str] = &[
    include_str!("../../migrations/20260513000000_create_job_queue/up.sql"),
    include_str!("../../migrations/20260515000000_create_repository_commit_hook_queue/up.sql"),
    include_str!("../../migrations/20260519000000_add_trace_context_to_jobs/up.sql"),
    include_str!("../../migrations/20260530300000_create_experiments/up.sql"),
    include_str!("../../migrations/20260610000000_add_job_uniqueness_concurrency/up.sql"),
    include_str!("../../migrations/20260611000000_add_pending_unique_key_to_jobs/up.sql"),
    include_str!("../../migrations/20260628000000_add_queue_to_jobs/up.sql"),
    include_str!("../../migrations/20260702000000_create_job_tracking/up.sql"),
    include_str!("../../migrations/20260831191622_retention_sweep_indexes/up.sql"),
    include_str!("../../migrations/20261022120000_add_version_to_job_tracking/up.sql"),
    include_str!("../../migrations/20261026170000_add_jobs_queue_name_ready_index/up.sql"),
    include_str!("../../migrations/20261112170000_add_jobs_unique_key_enqueued_index/up.sql"),
    include_str!(
        "../../migrations/20261115143207_add_commit_hooks_after_hook_recovery_index/up.sql"
    ),
];

const DROP: &str = "
DROP TABLE IF EXISTS autumn_jobs, autumn_repository_commit_hooks, autumn_job_tracking,
    autumn_experiment_assignments, autumn_experiment_overrides, autumn_experiments CASCADE;
DROP TYPE IF EXISTS autumn_experiment_state CASCADE;
";

/// Seconds of age for row `i` of `:rows`, spread over `:span` seconds,
/// oldest first, moved two hours older when it falls within an hour of
/// `:cut` seconds.
const AGE: &str = "(CASE WHEN :a BETWEEN :cut - 3600 AND :cut + 3600 THEN :a + 7200 ELSE :a END)";

fn age(span: i64, cut: i64) -> String {
    let raw = format!("(60 + (:rows - i)::bigint * {span} / :rows)");
    AGE.replace(":cut", &cut.to_string()).replace(":a", &raw)
}

const DAY: i64 = 86_400;

fn fill_jobs(rows: i64) -> String {
    let age = age(37 * DAY, 30 * DAY);
    format!(
        r#"
INSERT INTO autumn_jobs (id, name, payload, status, attempt, enqueued_at, started_at,
                         finished_at, last_error, run_at, unique_key, unique_window, queue)
SELECT md5('job-' || i), 'app::jobs::Job' || (1 + i % 12),
       jsonb_build_object('user_id', (i::bigint * 7919) % 100000, 'ref', md5(i::text)),
       s.st, CASE WHEN s.st = 'failed' THEN 5 ELSE 1 END,
       s.t, s.t + interval '1 second',
       CASE WHEN s.st IN ('enqueued', 'running') THEN NULL ELSE s.t + interval '2 seconds' END,
       CASE WHEN s.st IN ('failed', 'discarded')
            THEN 'error: upstream returned 503 after ' || (i % 30) || 's' END,
       s.t,
       CASE WHEN i % 20 = 7 THEN 'k-' || (i % 5000) END,
       CASE WHEN i % 20 = 7 THEN 'ttl' END,
       CASE WHEN i % 10 = 0 THEN 'mail' ELSE 'default' END
FROM (SELECT i, c.n - make_interval(secs => {age}) AS t,
             CASE WHEN i > :rows - :rows / 1000 AND i % 10 = 1 THEN 'running'
                  WHEN i > :rows - :rows / 1000 THEN 'enqueued'
                  WHEN i % 100 < 3 THEN 'failed'
                  WHEN i % 100 = 3 THEN 'discarded'
                  ELSE 'completed' END AS st
      FROM generate_series(1, :rows) AS i, (SELECT now() AS n) c) s;
UPDATE autumn_jobs SET attempt = attempt WHERE abs(hashtext(id)) % 100 < 4;
ANALYZE autumn_jobs;
"#
    )
    .replace(":rows", &rows.to_string())
}

fn fill_commit_hooks(rows: i64) -> String {
    let age = age(37 * DAY, 30 * DAY);
    format!(
        r#"
INSERT INTO autumn_repository_commit_hooks (id, handler_key, hook_name, context, record, status,
        attempt, enqueued_at, started_at, finished_at, claimed_at, last_error, run_at)
SELECT md5('hook-' || i), 'app::models::Post', (ARRAY['after_create', 'after_update', 'after_delete'])[1 + i % 3],
       jsonb_build_object('request_id', md5(i::text)),
       jsonb_build_object('id', i, 'title', 'post ' || i),
       s.st, 1, s.t, s.t + interval '1 second',
       CASE WHEN s.st IN ('completed', 'failed', 'after_hook_failed') THEN s.t + interval '2 seconds' END,
       CASE WHEN s.st IN ('running', 'pending_after_hook', 'after_hook_succeeded') THEN s.t END,
       CASE WHEN s.st IN ('failed', 'after_hook_failed') THEN 'hook returned an error' END,
       s.t
FROM (SELECT i, c.n - make_interval(secs => {age}) AS t,
             CASE WHEN i % 100 < 90 THEN 'completed'
                  WHEN i % 100 < 94 THEN 'failed'
                  WHEN i % 100 < 96 THEN 'after_hook_failed'
                  WHEN i % 100 = 96 THEN 'enqueued'
                  WHEN i % 100 = 97 THEN 'running'
                  WHEN i % 100 = 98 THEN 'pending_after_hook'
                  ELSE 'after_hook_succeeded' END AS st
      FROM generate_series(1, :rows) AS i, (SELECT now() AS n) c) s;
UPDATE autumn_repository_commit_hooks SET attempt = attempt WHERE abs(hashtext(id)) % 100 < 4;
ANALYZE autumn_repository_commit_hooks;
"#
    )
    .replace(":rows", &rows.to_string())
}

fn fill_job_tracking(rows: i64) -> String {
    let age = age(9 * DAY, 7 * DAY);
    format!(
        r#"
INSERT INTO autumn_job_tracking (key, record, updated_at, expires_at, version)
SELECT 'job-tracking:' || md5('job-' || i),
       jsonb_build_object('status', 'completed', 'progress', 100, 'result', md5(i::text)),
       s.t, s.t + interval '30 days', 1 + i % 3
FROM (SELECT i, c.n - make_interval(secs => {age}) AS t
      FROM generate_series(1, :rows) AS i, (SELECT now() AS n) c) s;
UPDATE autumn_job_tracking SET version = version WHERE abs(hashtext(key)) % 100 < 4;
ANALYZE autumn_job_tracking;
"#
    )
    .replace(":rows", &rows.to_string())
}

fn fill_experiments(rows: i64) -> String {
    let age = age(400 * DAY, 90 * DAY);
    format!(
        r#"
INSERT INTO autumn_experiments (name, state, variants, created_at, updated_at)
SELECT 'exp-' || i,
       (CASE WHEN i <= 24 THEN 'archived' WHEN i <= 30 THEN 'running'
             WHEN i <= 35 THEN 'concluded' ELSE 'draft' END)::autumn_experiment_state,
       '[{{"name":"control","weight":50}},{{"name":"treatment","weight":50}}]'::jsonb,
       now() - make_interval(days => 420 - i * 8), now()
FROM generate_series(1, 39) AS i;
INSERT INTO autumn_experiment_assignments (experiment, actor, variant, is_override, assigned_at)
SELECT 'exp-' || s.e, 'user-' || i, CASE WHEN i % 2 = 0 THEN 'control' ELSE 'treatment' END,
       i % 1000 = 0, s.t
FROM (SELECT i, c.n - make_interval(secs => {age}) AS t,
             -- 45% of rows to the 15 experiments the sweep keeps (25..39),
             -- the rest to the 24 archived ones and the deleted exp-40.
             CASE WHEN (i::bigint * 2654435761) % 100 < 45 THEN 25 + i % 15
                  ELSE 1 + i % 25 + (CASE WHEN i % 25 = 24 THEN 15 ELSE 0 END) END AS e
      FROM generate_series(1, :rows) AS i, (SELECT now() AS n) c) s;
UPDATE autumn_experiment_assignments SET is_override = is_override WHERE id % 100 < 4;
ANALYZE autumn_experiments;
ANALYZE autumn_experiment_assignments;
"#
    )
    .replace(":rows", &rows.to_string())
}

/// The datasets in the order the scheduled task runs them, with the table
/// each sweeps and the key that names a row.
const DATASETS: &[(&str, &str, &str)] = &[
    ("job_history", "autumn_jobs", "id"),
    ("commit_hooks", "autumn_repository_commit_hooks", "id"),
    ("job_tracking", "autumn_job_tracking", "key"),
    ("experiment_assignments", "autumn_experiment_assignments", "id::text"),
];

#[derive(QueryableByName)]
struct Count {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

#[derive(QueryableByName)]
struct Str {
    #[diesel(sql_type = Text)]
    s: String,
}

#[derive(QueryableByName)]
struct StatementRow {
    #[diesel(sql_type = Text)]
    query: String,
    #[diesel(sql_type = BigInt)]
    calls: i64,
    #[diesel(sql_type = BigInt)]
    rows: i64,
    #[diesel(sql_type = BigInt)]
    buffers: i64,
    #[diesel(sql_type = BigInt)]
    hit: i64,
    #[diesel(sql_type = BigInt)]
    temp: i64,
    #[diesel(sql_type = BigInt)]
    wal: i64,
}

async fn count(conn: &mut AsyncPgConnection, sql: &str) -> i64 {
    diesel::sql_query(sql)
        .get_result::<Count>(conn)
        .await
        .expect("count")
        .n
}

async fn text(conn: &mut AsyncPgConnection, sql: &str) -> String {
    diesel::sql_query(sql)
        .get_result::<Str>(conn)
        .await
        .expect("text")
        .s
}

/// `seq_scan`, `seq_tup_read`, `idx_scan`, `n_tup_del` of `table`, once
/// the other backends' flushes settle.
async fn table_counters(conn: &mut AsyncPgConnection, table: &str) -> [i64; 4] {
    let mut last = [-1; 4];
    for _ in 0..50 {
        conn.batch_execute("SELECT pg_stat_force_next_flush(); SELECT pg_stat_clear_snapshot();")
            .await
            .expect("flush stats");
        let mut now = [0; 4];
        for (slot, column) in ["seq_scan", "seq_tup_read", "idx_scan", "n_tup_del"]
            .iter()
            .enumerate()
        {
            now[slot] = count(
                conn,
                &format!(
                    "SELECT COALESCE({column}, 0)::bigint AS n FROM pg_stat_user_tables \
                     WHERE relname = '{table}'"
                ),
            )
            .await;
        }
        if now == last {
            return now;
        }
        last = now;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    last
}

async fn statements(conn: &mut AsyncPgConnection) -> Vec<StatementRow> {
    diesel::sql_query(
        "SELECT query, calls, rows, \
                (shared_blks_hit + shared_blks_read)::bigint AS buffers, \
                shared_blks_hit::bigint AS hit, \
                (temp_blks_read + temp_blks_written)::bigint AS temp, \
                wal_bytes::bigint AS wal \
         FROM pg_stat_statements \
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
           AND query NOT ILIKE '%pg_stat%' AND query NOT ILIKE 'CHECKPOINT%' \
         ORDER BY buffers DESC, calls DESC, query",
    )
    .load::<StatementRow>(conn)
    .await
    .expect("read pg_stat_statements")
}

fn print_statements(rows: &[StatementRow]) {
    println!(
        "{:>6} {:>8} {:>9} {:>9} {:>5} {:>11}  statement",
        "calls", "rows", "buffers", "hit", "temp", "wal_bytes"
    );
    let (mut calls, mut buffers, mut wal, mut temp) = (0, 0, 0, 0);
    for row in rows {
        let text: String = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        let short: String = text.chars().take(190).collect();
        println!(
            "{:>6} {:>8} {:>9} {:>9} {:>5} {:>11}  {short}",
            row.calls, row.rows, row.buffers, row.hit, row.temp, row.wal
        );
        calls += row.calls;
        buffers += row.buffers;
        wal += row.wal;
        temp += row.temp;
    }
    println!("-- total: statements={calls} buffers={buffers} temp={temp} statement_wal={wal} --");
}

fn config() -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.retention.job_history = Some("30d".to_owned());
    config.retention.commit_hooks = Some("30d".to_owned());
    config.retention.job_tracking = Some("7d".to_owned());
    config.retention.experiment_assignments = Some("90d".to_owned());
    config.jobs.tracking.ttl_secs = 365 * DAY.unsigned_abs();
    "postgres".clone_into(&mut config.jobs.backend);
    config
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
            "autovacuum=off",
            "-c",
            "shared_preload_libraries=pg_stat_statements",
            "-c",
            "pg_stat_statements.track=all",
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
async fn retention_sweep_profile() {
    let (url, _container) = server().await;
    let jobs: i64 = std::env::var("AUTUMN_LEDGER_RETENTION_JOBS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(2).build().expect("pool");
    let mut conn = pool.get().await.expect("conn");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .await
        .expect("pg_stat_statements");
    conn.batch_execute(DROP).await.expect("drop");
    for ddl in MIGRATIONS {
        conn.batch_execute(ddl).await.expect("migration");
    }
    conn.batch_execute(&fill_jobs(jobs)).await.expect("fill jobs");
    conn.batch_execute(&fill_commit_hooks(jobs / 4))
        .await
        .expect("fill commit hooks");
    conn.batch_execute(&fill_job_tracking(jobs / 4))
        .await
        .expect("fill job tracking");
    conn.batch_execute(&fill_experiments(jobs))
        .await
        .expect("fill experiments");

    println!(
        "server: {}",
        text(&mut conn, "SELECT version() AS s").await
    );
    for (_, table, _) in DATASETS {
        println!(
            "fixture: {table} rows={} dead={} heap={}MB indexes={}MB",
            count(&mut conn, &format!("SELECT count(*) AS n FROM {table}")).await,
            count(
                &mut conn,
                &format!(
                    "SELECT n_dead_tup AS n FROM pg_stat_user_tables WHERE relname = '{table}'"
                )
            )
            .await,
            count(&mut conn, &format!("SELECT pg_table_size('{table}') AS n")).await / 1_048_576,
            count(&mut conn, &format!("SELECT pg_indexes_size('{table}') AS n")).await
                / 1_048_576,
        );
    }

    let state = AppState::for_test().with_pool(pool.clone());
    state.insert_extension(config());
    for (dataset, table, key) in DATASETS {
        conn.batch_execute("CHECKPOINT; SELECT pg_stat_statements_reset()")
            .await
            .expect("reset pg_stat_statements");
        let before = table_counters(&mut conn, table).await;
        let reports = run_retention(
            &state,
            &RetentionRunOptions {
                dry_run: false,
                dataset: Some(dataset),
            },
        )
        .await
        .expect("sweep runs");
        let rows = statements(&mut conn).await;
        let after = table_counters(&mut conn, table).await;
        let report = reports.first().expect("one report");
        assert_eq!(report.error, None, "{report:?}");
        println!("\n=== {dataset} ({table}) ===");
        print_statements(&rows);
        println!(
            "-- report: eligible={:?} removed={} truncated={} --",
            report.eligible_rows, report.rows_removed, report.truncated
        );
        println!(
            "-- {table}: seq_scan={} seq_tup_read={} idx_scan={} n_tup_del={} --",
            after[0] - before[0],
            after[1] - before[1],
            after[2] - before[2],
            after[3] - before[3],
        );
        println!(
            "-- survivors: {} --",
            text(
                &mut conn,
                &format!(
                    "SELECT count(*) || ' rows, md5 ' || \
                            md5(COALESCE(string_agg({key}, ',' ORDER BY {key}), '')) AS s \
                     FROM {table}"
                )
            )
            .await
        );
    }
}
