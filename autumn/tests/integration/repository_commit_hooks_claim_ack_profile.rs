//! Ledger findings harness for the durable repository-commit-hook drain loop
//! (`drain_ready_repository_commit_hooks` in `autumn/src/repository_commit_hooks.rs`),
//! the dispatcher every `#[repository(hooks = ..., broadcasts = true)]`
//! mutation feeds through `autumn_repository_commit_hooks`.
//!
//! The loop claims up to `max_rows` ready hooks one at a time — each claim is
//! its own `pool.get()` + single-row `UPDATE ... FOR UPDATE SKIP LOCKED`
//! round trip — runs each hook's registered runner, then acks or nacks with a
//! second single-row `UPDATE`. This harness drives the REAL production drain
//! wiring (via the test-support export `autumn_web::test::drain_ready_repository_commit_hooks`,
//! which delegates straight to the private `drain_ready_repository_commit_hooks`
//! the background worker calls) against a production-shaped backlog, and
//! measures the claim/ack statement count via `pg_stat_statements`.
//!
//! **Findings issue, not a fix.** The obvious fix — claiming up to `max_rows`
//! ready hooks in a single `UPDATE ... WHERE id IN (SELECT ... LIMIT $n FOR
//! UPDATE SKIP LOCKED) RETURNING ...` round trip instead of `max_rows`
//! separate ones — changes queue *fairness*, not just its cost: today, a
//! worker claims one row, fully processes it (including waiting out its
//! heartbeat-guarded runner), then claims the next, so other replicas
//! draining the same `autumn_repository_commit_hooks` table (see the
//! migration's "another replica can retry work abandoned by a dead worker"
//! comment) can pick up rows 2..N while this worker is slow on row 1. Batch-
//! claiming locks all N rows to this worker up front, denying that work-
//! stealing window under multi-replica load. That is a deliberate concurrency
//! trade-off across replicas, not a pure "smallest change that moves the
//! counter" rewrite, so per the Ledger process it needs a human decision
//! rather than shipping silently in a PR.
//!
//! **Requires Docker.** CI runs it in the Docker-dependent sweep
//! (`-- --ignored`, see CLAUDE.md). Run manually with:
//!
//! ```text
//! cargo test -p autumn-web --features "test-support" \
//!   --test integration_tests -- --ignored repository_commit_hooks_claim_ack_profile \
//!   --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! The real `autumn_repository_commit_hooks` schema (from
//! `repository_commit_hook_migrations/20260515000000_create_repository_commit_hook_queue/up.sql`,
//! included verbatim so the fixture can never drift from the production
//! table/index shape). 200,000 `completed` historical rows (30-day age
//! spread) plus a 20,000-row `enqueued` ready backlog — a plausible one-shift
//! burst behind a slow/paused worker. Handler-key cardinality is skewed
//! 70/15/10/5 across four repositories, matching `examples/reddit-clone`'s
//! `PgPostRepository` (`broadcasts = true`) and its sibling repositories —
//! the real shape of an app where one hot repository dominates broadcast
//! traffic and a few others trail it.

#![cfg(feature = "db")]
// Buffer/statement counts here top out in the low hundred-thousands, nowhere
// near f64's 52-bit mantissa limit -- every `as f64` below is a display
// ratio/percentage, never a value compared against anything, so the lossy
// casts are harmless.
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const COMMIT_HOOK_UP: &str = include_str!(
    "../../repository_commit_hook_migrations/20260515000000_create_repository_commit_hook_queue/up.sql"
);

const HISTORICAL_ROWS: i64 = 200_000;
const BACKLOG_ROWS: i64 = 20_000;
const TOTAL_ROWS: i64 = HISTORICAL_ROWS + BACKLOG_ROWS;
const MAX_ROWS_PER_DRAIN: usize = 32;
const DRAIN_TICKS: usize = 10;

/// Skewed handler-key cardinality (70/15/10/5, matching a dominant broadcast
/// repository plus three trailing ones), 30-day age spread on the historical
/// `completed` rows, and a 20k `enqueued` ready backlog — the shape the
/// Ledger process requires (real row counts, real cardinality skew).
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO autumn_repository_commit_hooks \
         (id, handler_key, hook_name, status, attempt, enqueued_at, started_at, finished_at, run_at) \
         SELECT \
           'h_' || gs, \
           CASE WHEN gs % 100 < 70 THEN 'PgPostRepository' \
                WHEN gs % 100 < 85 THEN 'PgCommentRepository' \
                WHEN gs % 100 < 95 THEN 'PgVoteRepository' \
                ELSE 'PgUserRepository' END, \
           'create', 'completed', 1, \
           NOW() - (random() * interval '30 days'), \
           NOW() - (random() * interval '30 days'), \
           NOW() - (random() * interval '30 days'), \
           NOW() - (random() * interval '30 days') \
         FROM generate_series(1, {HISTORICAL_ROWS}) AS gs"
    ))
    .expect("seed historical completed hooks");

    conn.batch_execute(&format!(
        "INSERT INTO autumn_repository_commit_hooks \
         (id, handler_key, hook_name, status, attempt, enqueued_at, run_at) \
         SELECT \
           'e_' || gs, \
           CASE WHEN gs % 100 < 70 THEN 'PgPostRepository' \
                WHEN gs % 100 < 85 THEN 'PgCommentRepository' \
                WHEN gs % 100 < 95 THEN 'PgVoteRepository' \
                ELSE 'PgUserRepository' END, \
           'create', 'enqueued', 1, \
           NOW() - (random() * interval '2 minutes'), \
           NOW() - (random() * interval '2 minutes') \
         FROM generate_series(1, {BACKLOG_ROWS}) AS gs"
    ))
    .expect("seed enqueued backlog hooks");

    conn.batch_execute("ANALYZE autumn_repository_commit_hooks")
        .expect("analyze");
}

#[derive(QueryableByName, Debug)]
struct HandlerKeyCount {
    #[diesel(sql_type = Text)]
    handler_key: String,
    #[diesel(sql_type = BigInt)]
    n: i64,
}

/// Guards the fixture's advertised 70/15/10/5 handler-key skew: a review
/// caught (chatgpt-codex-connector on PR #2300) that an earlier version of
/// this seed generated its skew with `random()` inside a `LATERAL` subquery
/// that did not reference `gs`, so Postgres evaluated it once for the whole
/// `INSERT` instead of once per row -- every row silently landed in
/// whichever single bucket that one draw picked, instead of the advertised
/// skew. The fix (`gs % 100` thresholds, correlated with `gs` directly) is
/// deterministic, so this asserts the exact split rather than a tolerance.
fn assert_handler_key_skew(conn: &mut PgConnection) {
    use diesel::RunQueryDsl;
    let rows = diesel::sql_query(
        "SELECT handler_key, COUNT(*) AS n FROM autumn_repository_commit_hooks \
         GROUP BY handler_key ORDER BY n DESC",
    )
    .load::<HandlerKeyCount>(conn)
    .expect("query handler_key distribution");
    let total: i64 = TOTAL_ROWS;
    let expected: [(&str, i64); 4] = [
        ("PgPostRepository", (total * 70) / 100),
        ("PgCommentRepository", (total * 15) / 100),
        ("PgVoteRepository", (total * 10) / 100),
        ("PgUserRepository", (total * 5) / 100),
    ];
    for (handler_key, expected_n) in expected {
        let actual_n = rows
            .iter()
            .find(|row| row.handler_key == handler_key)
            .unwrap_or_else(|| panic!("{handler_key} must appear in the seeded fixture"))
            .n;
        assert_eq!(
            actual_n, expected_n,
            "{handler_key} must land exactly on its deterministic gs%100 share"
        );
    }
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

fn reset_stats(conn: &mut PgConnection) {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .expect("reset pg_stat_statements");
}

/// Prints every `autumn_repository_commit_hooks` statement from this run and
/// returns `(claim_calls, claim_buffers, ack_calls, ack_buffers,
/// precount_calls)` — the claim/ack statement shapes the real drain loop
/// issues, isolated from the test-harness export's own up-front `COUNT(*)`
/// (which the production background worker never issues — see
/// `drain_ready_repository_commit_hooks` in `autumn/src/test.rs`).
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%autumn_repository_commit_hooks%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut claim_calls, mut claim_buffers) = (0i64, 0i64);
    let (mut ack_calls, mut ack_buffers) = (0i64, 0i64);
    let mut precount_calls = 0i64;
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        if normalized.starts_with("SELECT COUNT(*)") {
            precount_calls += row.calls;
        } else if normalized.contains("FOR UPDATE SKIP LOCKED") {
            claim_calls += row.calls;
            claim_buffers += row.buffers;
        } else if normalized.starts_with("UPDATE autumn_repository_commit_hooks")
            && normalized.contains("finished_at = NOW()")
        {
            ack_calls += row.calls;
            ack_buffers += row.buffers;
        }
    }
    println!(
        "-- claim UPDATE: calls={claim_calls} buffers={claim_buffers} -- \
         ack UPDATE: calls={ack_calls} buffers={ack_buffers} -- \
         test-harness precount SELECT (not production cost): calls={precount_calls} --"
    );
    (
        claim_calls,
        claim_buffers,
        ack_calls,
        ack_buffers,
        precount_calls,
    )
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn repository_commit_hooks_claim_ack_profile() {
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

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .expect("create pg_stat_statements extension");
    conn.batch_execute(COMMIT_HOOK_UP)
        .expect("apply real repository-commit-hook queue migration");

    seed_fixture(&mut conn);
    assert_handler_key_skew(&mut conn);

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(config).build().expect("pool");

    // Register a runner for each seeded handler_key so the real claim query's
    // `handler_key = ANY($1)` (populated from the registered-handler-key
    // registry) matches the fixture, and the real ack path is exercised
    // (Ok(())  no side-effect I/O, to isolate claim/ack cost from hook-body
    // cost -- hook-body cost is a per-handler concern, not this queue's).
    for handler_key in [
        "PgPostRepository",
        "PgCommentRepository",
        "PgVoteRepository",
        "PgUserRepository",
    ] {
        autumn_web::__private::register_repository_commit_hook_runner(
            handler_key,
            |_ctx, _record| async { Ok(()) },
            |_ctx, _record| async { Ok(()) },
            |_ctx, _record| async { Ok(()) },
        );
    }

    // EXPLAIN a single claim in isolation first, on the untouched backlog --
    // a diagnostic, not the scale claim.
    explain(
        &mut conn,
        "single claim, cold backlog (20,000 enqueued, 200,000 completed history)",
        "UPDATE autumn_repository_commit_hooks \
         SET status = 'running', started_at = NOW(), claimed_by = 'diag', claimed_at = NOW() \
         WHERE id = ( \
           SELECT id FROM autumn_repository_commit_hooks \
           WHERE status = 'enqueued' AND run_at <= NOW() \
             AND handler_key = ANY(ARRAY['PgPostRepository','PgCommentRepository','PgVoteRepository','PgUserRepository']) \
           ORDER BY run_at ASC, enqueued_at ASC LIMIT 1 FOR UPDATE SKIP LOCKED \
         ) RETURNING id",
    );

    // Roll the diagnostic claim back so the workload run below starts clean.
    conn.batch_execute(
        "UPDATE autumn_repository_commit_hooks SET status = 'enqueued', started_at = NULL, \
         claimed_by = NULL, claimed_at = NULL WHERE claimed_by = 'diag'",
    )
    .expect("reset diagnostic claim");

    // Workload: DRAIN_TICKS calls to the real drain export, each capped at
    // MAX_ROWS_PER_DRAIN -- exactly how the background worker's kick loop
    // calls `drain_ready_repository_commit_hooks` (see
    // `subscribe_repository_commit_hook_worker` in
    // `autumn/src/repository_commit_hooks.rs`), just invoked directly instead
    // of via the timing-based `Notify` signal.
    reset_stats(&mut conn);
    let mut total_processed = 0usize;
    for _ in 0..DRAIN_TICKS {
        let processed =
            autumn_web::test::drain_ready_repository_commit_hooks(&pool, MAX_ROWS_PER_DRAIN).await;
        total_processed += processed;
    }
    assert_eq!(
        total_processed,
        DRAIN_TICKS * MAX_ROWS_PER_DRAIN,
        "backlog (20,000) must stay deep enough that every tick claims a full batch"
    );

    let (claim_calls, claim_buffers, ack_calls, ack_buffers, precount_calls) = print_profile(
        &mut conn,
        &format!(
            "{DRAIN_TICKS} drain ticks x {MAX_ROWS_PER_DRAIN} hooks = {total_processed} hooks processed"
        ),
    );

    assert_eq!(
        claim_calls, total_processed as i64,
        "current code: exactly one claim round trip per hook processed"
    );
    assert_eq!(
        ack_calls, total_processed as i64,
        "every processed hook acks (registered runners always return Ok)"
    );
    assert_eq!(
        precount_calls, DRAIN_TICKS as i64,
        "the test-harness export's own precount runs once per drain call, not per hook"
    );

    let total_statements = claim_calls + ack_calls;
    let batched_claim_statements = DRAIN_TICKS as i64 + ack_calls;
    let reduction_pct = (1.0 - (batched_claim_statements as f64 / total_statements as f64)) * 100.0;

    println!(
        "\n=== statement-count summary ({DRAIN_TICKS} drain ticks, {MAX_ROWS_PER_DRAIN} rows/tick cap) ==="
    );
    println!("current:  {claim_calls} claim + {ack_calls} ack = {total_statements} statements");
    println!(
        "if claim were batched (1 UPDATE...LIMIT {MAX_ROWS_PER_DRAIN} RETURNING per tick): \
         {DRAIN_TICKS} claim + {ack_calls} ack = {batched_claim_statements} statements \
         ({reduction_pct:.1}% fewer statements)"
    );
    println!(
        "claim buffers: {claim_buffers} total, {:.1}/call -- ack buffers: {ack_buffers} total, {:.1}/call",
        claim_buffers as f64 / claim_calls.max(1) as f64,
        ack_buffers as f64 / ack_calls.max(1) as f64
    );
    println!(
        "test-harness precount overhead: {precount_calls} calls -- NOT part of the production \
         worker's cost (the real `subscribe_repository_commit_hook_worker` loop never precounts)"
    );

    // Demonstrate the batched-claim alternative's plan/buffer shape too, so
    // the fairness trade-off write-up isn't arguing from theory alone --
    // rolled back so it doesn't disturb the recorded workload numbers above.
    conn.transaction::<(), diesel::result::Error, _>(|conn| {
        explain(
            conn,
            "hypothetical batched claim: one UPDATE...LIMIT 32 RETURNING (rolled back, not applied)",
            &format!(
                "UPDATE autumn_repository_commit_hooks \
                 SET status = 'running', started_at = NOW(), claimed_by = 'diag-batch', claimed_at = NOW() \
                 WHERE id IN ( \
                   SELECT id FROM autumn_repository_commit_hooks \
                   WHERE status = 'enqueued' AND run_at <= NOW() \
                     AND handler_key = ANY(ARRAY['PgPostRepository','PgCommentRepository','PgVoteRepository','PgUserRepository']) \
                   ORDER BY run_at ASC, enqueued_at ASC LIMIT {MAX_ROWS_PER_DRAIN} FOR UPDATE SKIP LOCKED \
                 ) RETURNING id"
            ),
        );
        Err(diesel::result::Error::RollbackTransaction)
    })
    .ok();
}
