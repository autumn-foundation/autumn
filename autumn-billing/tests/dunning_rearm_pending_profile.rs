//! Ledger findings harness for `dunning::rearm_pending`
//! (`autumn-billing/src/dunning.rs`), the step every app-process startup
//! takes to re-queue whatever dunning retries were still open when the
//! process last stopped — "a restart re-arms from the store" (module doc
//! comment).
//!
//! `rearm_pending` loads every open (`Pending`/`Running`) row via
//! `BillingStore::open_dunning()` — one round trip, correctly unscoped: this
//! is the one place in the codebase that legitimately needs every open row,
//! system-wide, not the over-broad read the 09-11 `close_dunning_for` fix
//! (`docs/reports/2026-09-11-ledger-dunning-close-scan-scoped/`) scoped down
//! for the request path. The defect is on the WRITE side, untouched by that
//! fix: `rearm_row` (dunning.rs) calls `schedule()` → `JobClient::enqueue_due`
//! once per row, sequentially awaited
//! (`for row in rows { rearm_row(&state, &row).await }`), so re-arming N open
//! rows costs N sequential `INSERT INTO autumn_jobs` round trips. Production
//! `rearm_pending` spawns this loop (`tokio::runtime::Handle::spawn`) rather
//! than awaiting it, and `run_startup_hooks` never sees that spawned task —
//! it awaits only the `on_startup` closure's own future, which returns as
//! soon as the spawn call does — so this does NOT delay
//! `ProbeState::mark_startup_complete()` or readiness. What it does cost:
//! every one of these N round trips runs against the same connection pool
//! and Postgres instance a freshly-restarted process is about to start
//! serving live traffic through, and no retry in the backlog is actually
//! re-queued until its row's turn in the sequential loop comes up — so a
//! large backlog extends how long the *payment-retry recovery itself* takes
//! after a restart, even though the process reports itself ready well
//! before that finishes.
//!
//! This is a **findings issue** harness, not a before/after fix. The
//! mechanism is identical to the one already filed and deliberately left
//! unfixed in `autumn/tests/integration/webhook_outbound_dispatch_fanout_profile.rs`:
//! `JobClient::enqueue`/`enqueue_due` is not a thin `INSERT` wrapper — per
//! logical call it also evaluates the uniqueness dedup subquery
//! (`ON CONFLICT ... DO NOTHING`, with a TTL-window eviction step; the dunning
//! retry job declares `unique_by = "invoice_id"`, so this is live here, not
//! hypothetical), captures OTLP trace context, invokes any registered
//! `JobInterceptor`, and updates the in-process `JobRegistry` counters — all
//! before the row is written. Collapsing that into one batched round trip
//! would have to happen inside `JobClient` itself (a new `enqueue_many`) to
//! help this call site *and* the webhook one, and is the same job-queue-wide
//! API/semantics decision that finding already routed to a human rather than
//! shipping silently. A narrower, crate-local "just batch the INSERT" fix
//! would mean re-implementing the dedup/interceptor/tracing logic in
//! `autumn-billing` by hand — the exact correctness risk that finding
//! rejected the same trade for.
//!
//! This harness adds a second, real measured data point for that decision:
//! unlike the webhook fan-out (triggered per business event, at whatever rate
//! the app receives them), this N+1 runs **every single process restart**,
//! and specifically grows with an *upstream outage* — the one moment a
//! payment provider incident is already degrading service, every redeploy or
//! crash-restart during the incident pays for the full backlog again,
//! sequentially, in the background — not blocking startup (see above), but
//! extending how long that backlog stays un-re-armed.
//!
//! Exercises the real production path: `dunning::rearm_pending_now` is the
//! exact body `dunning::rearm_pending` (the private startup-hook entry point
//! `BillingPlugin::build` calls) spawns — factored out and exposed `pub`
//! specifically so this harness can await its completion deterministically
//! instead of racing a spawned task, the same reason
//! `autumn_web::test::drain_ready_repository_commit_hooks` exposes the
//! repository-commit-hooks drain loop for its own Ledger harness
//! (`repository_commit_hooks_claim_ack_profile.rs`).
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p autumn-billing --test dunning_rearm_pending_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! This crate has no consolidated Docker sweep (see CLAUDE.md) — this binary
//! needs, and has, the same explicit `--test dunning_rearm_pending_profile
//! -- --ignored` line in `ci.yml`'s Docker-dependent step and its coverage
//! step, right next to the sibling `dunning_close_scan_profile` line.
//!
//! ## Fixture
//!
//! 20,000 customers, each with one subscription — the same base population
//! `dunning_close_scan_profile.rs` uses. One in seven (`gs % 7 == 0`, ~2,857
//! rows, disjoint invoice-id namespace) has a **closed** historical dunning
//! row (`recovered`/`exhausted`), giving `billing_dunning` a realistic
//! long-tail size and real dead tuples (a follow-up `UPDATE` before `ANALYZE`,
//! no `VACUUM`) — matching every other Ledger fixture in this repo, even
//! though this harness's target statement never touches those rows.
//!
//! Three cumulative backlog tiers model a payment-provider incident widening
//! across three consecutive restarts: 50 open rows (a brief blip), then +450
//! more (499 total, a sustained partial outage), then +1,500 more (1,999
//! total, a major processor-wide incident affecting ~10% of the customer
//! base). Each tier's open rows are seeded, `pg_stat_statements` is reset, and
//! `rearm_pending_now` runs against the store's *entire* open set each time —
//! matching the real "restart re-arms everything still open" contract, so the
//! later tiers' call counts include the earlier tiers' still-open rows, not
//! just that tier's new ones.

// Row/statement counts here top out in the low thousands, nowhere near f64's
// 52-bit mantissa limit -- every `as f64` below is a display ratio, never a
// value compared against anything. The harness's single `#[tokio::test]`
// body is long because it is the linear seed -> fire -> profile pipeline
// this Ledger process requires in one place, not because it is doing several
// unrelated things.
#![allow(clippy::cast_precision_loss, clippy::too_many_lines)]

#[path = "cases/support.rs"]
mod support;

use std::sync::Arc;

use autumn_billing::{BillingService, DbBillingStore, NoHooks};
use autumn_web::AppState;
use autumn_web::config::JobConfig;
use autumn_web::job;
use autumn_web::reexports::diesel_migrations::MigrationHarness;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// Real production job-queue schema, in order -- matches CLAUDE.md's own
// requirement that a fixture's schema can never drift from what `autumn
// migrate` actually applies. Same set `webhook_outbound_dispatch_fanout_profile.rs`
// uses for the same reason (job_tracking's own migration is intentionally
// left out: the default tracking store is in-memory, per
// `job_tracking::store_for_config`, so nothing in this profile touches it).
const CREATE_AUTUMN_JOBS: &str =
    include_str!("../../autumn/migrations/20260513000000_create_job_queue/up.sql");
const ADD_TRACE_CONTEXT_TO_JOBS: &str =
    include_str!("../../autumn/migrations/20260519000000_add_trace_context_to_jobs/up.sql");
const ADD_JOB_UNIQUENESS_CONCURRENCY: &str =
    include_str!("../../autumn/migrations/20260610000000_add_job_uniqueness_concurrency/up.sql");
const ADD_PENDING_UNIQUE_KEY_TO_JOBS: &str =
    include_str!("../../autumn/migrations/20260611000000_add_pending_unique_key_to_jobs/up.sql");
const ADD_QUEUE_TO_JOBS: &str =
    include_str!("../../autumn/migrations/20260628000000_add_queue_to_jobs/up.sql");

const TOTAL_SUBSCRIPTIONS: i64 = 20_000;
/// Every 7th subscription has a closed historical dunning row (disjoint
/// invoice-id namespace) -- the same `CLOSED_STRIDE` `dunning_close_scan_profile.rs`
/// uses, for the same reason: realistic long-tail table size and dead tuples.
const CLOSED_STRIDE: i64 = 7;
/// Cumulative open-row count at the end of each of the three restart tiers.
const TIERS: [i64; 3] = [50, 500, 2_000];

/// Customers + subscriptions (1:1, none with an open dunning row yet) plus
/// the closed historical dunning long tail, with real dead tuples from a
/// follow-up `UPDATE`+`ANALYZE` and no `VACUUM` -- the fixture shape the
/// Ledger process requires.
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO billing_customers (id, provider, provider_customer_id, created_at, updated_at) \
         SELECT 'cust_' || gs, 'stripe', 'cus_stripe_' || gs, \
           TIMESTAMP '2025-01-01 00:00:00' + (gs || ' seconds')::interval, \
           TIMESTAMP '2025-01-01 00:00:00' + (gs || ' seconds')::interval \
         FROM generate_series(1, {TOTAL_SUBSCRIPTIONS}) AS gs"
    ))
    .expect("seed billing_customers");

    conn.batch_execute(&format!(
        "INSERT INTO billing_subscriptions \
         (id, customer_id, provider_subscription_id, status, quantity, cancel_at_period_end, \
          last_event_at, created_at, updated_at) \
         SELECT 'sub_' || gs, 'cust_' || gs, 'sub_stripe_' || gs, \
           CASE WHEN gs % 11 = 0 THEN 'past_due' ELSE 'active' END, \
           1, 0, \
           TIMESTAMP '2025-06-01 00:00:00', \
           TIMESTAMP '2025-01-01 00:00:00' + (gs || ' seconds')::interval, \
           TIMESTAMP '2025-06-01 00:00:00' \
         FROM generate_series(1, {TOTAL_SUBSCRIPTIONS}) AS gs"
    ))
    .expect("seed billing_subscriptions");

    conn.batch_execute(&format!(
        "INSERT INTO billing_dunning \
         (invoice_id, customer_id, subscription_id, attempt, next_attempt_at, state, updated_at) \
         SELECT 'inv_closed_' || gs, 'cust_' || gs, 'sub_' || gs, \
           2, \
           TIMESTAMP '2025-03-01 00:00:00' + (gs || ' minutes')::interval, \
           CASE WHEN gs % 21 = 0 THEN 'exhausted' ELSE 'recovered' END, \
           TIMESTAMP '2025-03-02 00:00:00' \
         FROM generate_series(1, {TOTAL_SUBSCRIPTIONS}) AS gs \
         WHERE gs % {CLOSED_STRIDE} = 0"
    ))
    .expect("seed closed billing_dunning rows");

    conn.batch_execute(
        "UPDATE billing_dunning SET updated_at = NOW() WHERE state IN ('recovered', 'exhausted')",
    )
    .expect("create dead tuples");
    conn.batch_execute("ANALYZE billing_customers")
        .expect("analyze customers");
    conn.batch_execute("ANALYZE billing_subscriptions")
        .expect("analyze subscriptions");
    conn.batch_execute("ANALYZE billing_dunning")
        .expect("analyze dunning");
}

/// Opens a new dunning row (attempt 1, `Pending`, due now) for every
/// subscription `gs` in `from..=to` -- the next restart tier's incremental
/// backlog. Distinct from the closed set's namespace (`inv_` vs
/// `inv_closed_`), and `from..=to` never overlaps a prior call's range, so
/// each call adds exactly `to - from + 1` new open rows.
fn seed_open_backlog(conn: &mut PgConnection, from: i64, to: i64) {
    conn.batch_execute(&format!(
        "INSERT INTO billing_dunning \
         (invoice_id, customer_id, subscription_id, attempt, next_attempt_at, state, updated_at) \
         SELECT 'inv_' || gs, 'cust_' || gs, 'sub_' || gs, \
           1, NOW(), 'pending', NOW() \
         FROM generate_series({from}, {to}) AS gs"
    ))
    .expect("seed open backlog rows");
    conn.batch_execute("ANALYZE billing_dunning")
        .expect("re-analyze dunning after backlog growth");
}

#[derive(QueryableByName, Debug)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

fn count(conn: &mut PgConnection, sql: &str) -> i64 {
    use diesel::RunQueryDsl;
    diesel::sql_query(sql)
        .get_result::<CountRow>(conn)
        .expect("count query")
        .n
}

fn reset_stats(conn: &mut PgConnection) {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .expect("reset pg_stat_statements");
}

#[derive(QueryableByName, Debug)]
struct StatementRow {
    #[diesel(sql_type = Text)]
    query: String,
    #[diesel(sql_type = BigInt)]
    calls: i64,
    #[diesel(sql_type = BigInt)]
    buffers: i64,
}

/// Every `pg_stat_statements` query below excludes this fragment too, unique
/// to `pg_update_queue_depth_gauges`'s `SELECT ... FROM autumn_jobs ...
/// GROUP BY queue, name` (`autumn/src/job.rs`). `job::start_runtime` spawns
/// that survey on a fixed 5-second interval on **every** role, including the
/// enqueue-only (`run_workers = false`) one this harness starts, specifically
/// so a web replica's `/actuator/jobs` gauges reflect the shared backlog —
/// it has nothing to do with dunning, runs whether or not a restart ever
/// re-arms anything, and its own interval fires immediately on spawn and
/// every 5s after, landing at an arbitrary point relative to this harness's
/// `reset_stats`/measure windows (confirmed: an earlier version of this
/// harness saw its `oldest_wait_ms` statement show up with 0, 4, then 1,624
/// buffers across the three tiers, pure scheduling noise). Cancelling the
/// runtime's `shutdown` token after `start_runtime` returns does not close
/// this race either -- the survey's first tick can already be in flight on
/// another worker thread by the time cancellation is observed -- so this
/// filters the statement out of the measurement instead of racing it.
const EXCLUDE_QUEUE_DEPTH_SURVEY: &str = "query NOT ILIKE '%oldest_wait_ms%'";

/// Prints every statement this run issued, ranked by buffers then by calls.
/// Returns `(job_insert_calls, job_insert_buffers, workload_total_buffers)` --
/// the `INSERT INTO autumn_jobs` statement this harness targets, isolated
/// from the one `open_dunning` `SELECT` and the `prune_events` `DELETE`
/// (neither of which scale with backlog size the way the insert loop does),
/// and the workload's grand total buffers across every statement, for the
/// profile percentage.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} (by buffers) ===");
    let by_buffers = diesel::sql_query(format!(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_statements%' AND {EXCLUDE_QUEUE_DEPTH_SURVEY} \
         ORDER BY buffers DESC LIMIT 10"
    ))
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements by buffers");
    for row in &by_buffers {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<10} {normalized}",
            row.calls, row.buffers
        );
    }

    println!("\n=== pg_stat_statements: {label} (by calls) ===");
    let by_calls = diesel::sql_query(format!(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_statements%' AND {EXCLUDE_QUEUE_DEPTH_SURVEY} \
         ORDER BY calls DESC LIMIT 10"
    ))
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements by calls");
    for row in &by_calls {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<10} {normalized}",
            row.calls, row.buffers
        );
    }

    let grand_total_all: i64 = diesel::sql_query(format!(
        "SELECT COALESCE(SUM(shared_blks_hit + shared_blks_read), 0)::bigint AS n \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_statements%' AND {EXCLUDE_QUEUE_DEPTH_SURVEY}"
    ))
    .get_result::<CountRow>(conn)
    .expect("grand total buffers")
    .n;

    let target_rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE 'INSERT INTO autumn_jobs%'",
    )
    .load::<StatementRow>(conn)
    .expect("query the target statement");
    assert!(
        target_rows.len() <= 1,
        "expected at most one job-insert statement shape, found {}: {target_rows:?}",
        target_rows.len()
    );
    let (target_calls, target_buffers) =
        target_rows.first().map_or((0, 0), |r| (r.calls, r.buffers));
    println!(
        "\n-- target INSERT: calls={target_calls} buffers={target_buffers} \
         (workload total buffers={grand_total_all}) --"
    );
    (target_calls, target_buffers, grand_total_all)
}

#[derive(QueryableByName, Debug)]
struct ExplainLine {
    #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
    line: String,
}

fn explain(conn: &mut PgConnection, label: &str, sql: &str) {
    use diesel::RunQueryDsl;
    println!("\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): {label} ===");
    println!("{sql}");
    let lines = diesel::sql_query(format!(
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) {sql}"
    ))
    .load::<ExplainLine>(conn)
    .expect("explain");
    for line in lines {
        println!("{}", line.line);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker (testcontainers)"]
async fn dunning_rearm_pending_profile() {
    let _guard = job::global_job_runtime_test_lock().lock().await;
    job::clear_global_job_client();

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
            "pg_stat_statements.max=2000",
        ])
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let migrate_url = url.clone();
    tokio::task::spawn_blocking(move || {
        let mut conn = diesel::PgConnection::establish(&migrate_url).expect("sync connection");
        conn.run_pending_migrations(autumn_billing::MIGRATIONS)
            .expect("apply billing migrations");
    })
    .await
    .expect("migration task");

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .expect("create pg_stat_statements extension");
    conn.batch_execute(CREATE_AUTUMN_JOBS)
        .expect("apply create_job_queue migration");
    conn.batch_execute(ADD_TRACE_CONTEXT_TO_JOBS)
        .expect("apply add_trace_context_to_jobs migration");
    conn.batch_execute(ADD_JOB_UNIQUENESS_CONCURRENCY)
        .expect("apply add_job_uniqueness_concurrency migration");
    conn.batch_execute(ADD_PENDING_UNIQUE_KEY_TO_JOBS)
        .expect("apply add_pending_unique_key_to_jobs migration");
    conn.batch_execute(ADD_QUEUE_TO_JOBS)
        .expect("apply add_queue_to_jobs migration");

    seed_fixture(&mut conn);

    let closed_total = count(
        &mut conn,
        "SELECT COUNT(*) AS n FROM billing_dunning WHERE state IN ('recovered', 'exhausted')",
    );
    println!(
        "\n-- fixture: {TOTAL_SUBSCRIPTIONS} subscriptions, {closed_total} closed historical \
         dunning rows, 0 open rows yet --"
    );

    let manager_config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url.clone());
    let pool = Pool::builder(manager_config).build().expect("pool");

    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();
    let job_config = JobConfig {
        backend: "postgres".to_owned(),
        ..Default::default()
    };
    // `run_workers = false`: this harness measures the re-arm enqueue leg
    // only. No worker claims/processes the rows, so claim/ack machinery
    // never contaminates the `autumn_jobs` statement profile below.
    job::start_runtime(
        autumn_billing::dunning::job_infos(),
        &state,
        &shutdown,
        &job_config,
        false,
    )
    .expect("start postgres-backed job runtime");

    let store: Arc<dyn autumn_billing::BillingStore> = Arc::new(DbBillingStore::new(pool));
    let provider = support::FakeProvider::new();
    let service = Arc::new(BillingService::new(
        provider,
        store,
        support::catalog(),
        support::config(),
        Arc::new(NoHooks),
    ));

    let mut cumulative_open = 0i64;
    let mut tier_results = Vec::new();
    for &target in &TIERS {
        let from = cumulative_open + 1;
        let to = target;
        seed_open_backlog(&mut conn, from, to);
        cumulative_open = target;

        let open_total = count(
            &mut conn,
            "SELECT COUNT(*) AS n FROM billing_dunning WHERE state IN ('pending', 'running')",
        );
        assert_eq!(
            open_total, cumulative_open,
            "backlog should now hold exactly this tier's cumulative open-row count"
        );

        reset_stats(&mut conn);
        let armed = autumn_billing::dunning::rearm_pending_now(&state, &service).await;
        assert_eq!(
            armed,
            usize::try_from(cumulative_open).expect("fits in usize"),
            "rearm_pending_now must re-arm every currently-open row, tier target={target}"
        );

        let (target_calls, target_buffers, workload_buffers) = print_profile(
            &mut conn,
            &format!("restart re-arming {cumulative_open} open rows"),
        );
        assert_eq!(
            target_calls, cumulative_open,
            "rearm issues exactly one INSERT INTO autumn_jobs per open row, sequentially"
        );
        let buffers_pct = 100.0 * target_buffers as f64 / workload_buffers.max(1) as f64;
        println!(
            "-- job-insert loop: calls={target_calls} buffers={target_buffers} \
             ({buffers_pct:.1}% of {workload_buffers} total restart-workload buffers) --"
        );
        tier_results.push((
            cumulative_open,
            target_calls,
            target_buffers,
            workload_buffers,
        ));
    }

    println!("\n=== restart-tier statement-count scaling ===");
    println!(
        "{:<12} {:>12} {:>14} {:>18}",
        "open rows", "insert calls", "insert buffers", "workload buffers"
    );
    for (open_rows, calls, buffers, workload) in &tier_results {
        println!("{open_rows:<12} {calls:>12} {buffers:>14} {workload:>18}");
    }

    // Illustrative EXPLAIN of the two shapes -- the per-row insert this
    // harness measures, run today once per open row, vs. what a batched
    // `enqueue_many` (not implemented; the job-queue-wide API/semantics
    // decision this finding routes to a human) would let this call site
    // issue instead: one multi-row `INSERT ... SELECT ... FROM unnest(...)`.
    // Diagnostic only, not the scale claim -- the scale claim is the
    // pg_stat_statements table above.
    explain(
        &mut conn,
        "one row's job insert, issued once per open dunning row today",
        "INSERT INTO autumn_jobs \
         (id, name, queue, payload, status, attempt, max_attempts, initial_backoff_ms, \
          enqueued_at, run_at) \
         SELECT 'illustrative-single', 'autumn_billing_dunning_retry', 'billing', '{}'::JSONB, \
           'enqueued', 1, 5, 60000, NOW(), NOW() \
         RETURNING id",
    );
    explain(
        &mut conn,
        "20 rows' job inserts batched into one multi-row INSERT (illustrative; not implemented)",
        "INSERT INTO autumn_jobs \
         (id, name, queue, payload, status, attempt, max_attempts, initial_backoff_ms, \
          enqueued_at, run_at) \
         SELECT 'illustrative-' || gs, 'autumn_billing_dunning_retry', 'billing', '{}'::JSONB, \
           'enqueued', 1, 5, 60000, NOW(), NOW() \
         FROM generate_series(1, 20) AS gs \
         RETURNING id",
    );

    job::clear_global_job_client();
}
