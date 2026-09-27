//! Ledger findings harness for `WebhookOutboundManager::dispatch`
//! (`autumn/src/webhook_outbound.rs`), the public fan-out entry point an app
//! calls once per business event to notify every subscriber interested in a
//! topic (the "tell every registered consumer that `order.created`" shape).
//!
//! `dispatch` loops the topic's active subscriptions and, per subscriber,
//! awaits `handler.log_delivery(...)` then — absent a Harvest delegate
//! extension, the shipped default — `job_client.enqueue("autumn_webhook_delivery", ...)`
//! (`webhook_outbound.rs` ~410-471). `OutboundWebhookStore`/`OutboundWebhookHandler`
//! is a pluggable, BYO-persistence trait; **no Postgres-backed implementation
//! ships anywhere in this repo** (not in `autumn`, not in `examples/reddit-clone`,
//! whose own webhook integration test uses `InMemoryOutboundWebhookStore` —
//! see `docs/guide/outbound-webhooks.md` section 2). So `log_delivery` is not
//! a database call in any committed deployment shape; the one piece of real,
//! measurable Postgres traffic this path generates is the fallback
//! `job_client.enqueue()` call — one `INSERT INTO autumn_jobs` per active
//! subscriber, sequentially awaited, every dispatch.
//!
//! This harness drives the **real** `WebhookOutboundManager::dispatch` and
//! the real Postgres-backed `JobClient::enqueue` path (`job.rs`'s
//! `pg_insert_job`) against the production `autumn_jobs` schema, with workers
//! disabled (`run_workers = false`) so enqueued rows are measured, not also
//! claimed/processed in the same window.
//!
//! **Findings issue, not a fix.** Collapsing N sequential `enqueue()` calls
//! into one batched multi-row `INSERT` would have to happen inside
//! `JobClient::enqueue` (or a new `enqueue_many`) to help every caller, not
//! just this one — and `enqueue` is not a thin insert wrapper: it also
//! evaluates per-job uniqueness dedup (`ON CONFLICT (name, unique_key) ...
//! DO NOTHING`, with a TTL-window eviction step before the INSERT), captures
//! OTLP trace context per row, calls a registered `JobInterceptor` once per
//! logical job (capsule replay/fault-injection seam, #1634), and updates the
//! in-process `JobRegistry`/`JobAdminMemoryBackend` counters synchronously
//! per call, all before the row is written. `autumn_webhook_delivery` itself
//! declares no uniqueness or concurrency (so this dispatch loop happens to
//! skip the dedup-guard subquery), but a shared batched path used by every
//! job caller cannot assume that — it would have to fold N interceptor
//! invocations, N trace-context captures, and a per-row unique-dedup/ON
//! CONFLICT decision into one round trip without changing what any of those
//! observe. That is a job-queue-wide API and semantics decision (the same
//! category of call the Ledger process already routed to a human for the
//! structurally identical `WebPush::send_many` subscription-lookup finding,
//! PR #2446, and the `repository_commit_hooks` claim/ack finding, PR #2300),
//! not a "smallest change that moves the counter" rewrite scoped to this one
//! call site.
//!
//! **Requires Docker.** CI runs it in the Docker-dependent sweep
//! (`-- --ignored`, see CLAUDE.md). Run manually with:
//!
//! ```text
//! cargo test -p autumn-web --features "db,test-support" \
//!   --test integration_tests -- --ignored webhook_outbound_dispatch_fanout_profile \
//!   --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! The real `autumn_jobs` schema, built from the same migration set the
//! production migration runner applies, in order (`create_job_queue`,
//! `add_trace_context_to_jobs`, `add_job_uniqueness_concurrency`,
//! `add_pending_unique_key_to_jobs`, `add_queue_to_jobs`) — included verbatim
//! so the fixture can never drift from the production table/index shape,
//! matching the precedent set by the `repository_commit_hooks_claim_ack_profile`
//! harness (PR #2300). Before dispatching anything, the table is seeded with
//! 250,000 historical rows spanning `completed`, `failed`, and `enqueued`
//! (85/10/5) across a 30-day age spread and four other job names, standing in
//! for a busy app's real job history/backlog the INSERT's dedup-guard
//! subquery and indexes have to work around — a 1,000-row `autumn_jobs`
//! table tells you nothing about the index/bloat shape the planner sees in
//! production.
//!
//! `dispatch` is called with three subscriber-fan-out sizes standing in for a
//! handful of integration partners, a mid-size platform, and a large
//! multi-tenant integrator — 25, 250, and 2,500 active subscriptions on the
//! dispatched topic — plus a disabled subscription mixed into each tier (10%)
//! that must be skipped without costing an enqueue, matching `dispatch`'s own
//! `WebhookSubscriptionStatus::Disabled` guard.

#![cfg(feature = "db")]
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    // `AllStatusHandler` mirrors `InMemoryOutboundWebhookHandler`'s own
    // `get_subscriptions`/`get_subscription` shape (`webhook_outbound.rs`),
    // which allows this same nursery lint at the module level for the same
    // read-guard-then-build-list pattern.
    clippy::significant_drop_tightening
)]

use autumn_web::config::JobConfig;
use autumn_web::job::{self, JobInfo};
use autumn_web::webhook_outbound::{
    OutboundWebhookHandler, WebhookDeliveryLog, WebhookOutboundManager, WebhookSubscription,
    WebhookSubscriptionStatus,
};
use autumn_web::{AppState, AutumnResult};
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

/// A test-only handler that returns every subscription matching the topic
/// **regardless of status**. `InMemoryOutboundWebhookHandler::get_subscriptions`
/// already filters to `Active` per its own doc contract ("Retrieve active
/// subscriptions..."), so seeding disabled subscriptions into it never
/// exercises `dispatch`'s own `WebhookSubscriptionStatus::Disabled` skip
/// (`webhook_outbound.rs` `dispatch()` ~412-414) — the handler would have
/// already dropped them, making a "disabled subs cost zero enqueues"
/// assertion vacuous (caught by Codex review on PR #2532). This handler
/// hands disabled subscriptions through so dispatch's own guard is the thing
/// actually under test.
#[derive(Default)]
struct AllStatusHandler {
    subscriptions: RwLock<HashMap<String, WebhookSubscription>>,
    logs: RwLock<HashMap<String, WebhookDeliveryLog>>,
}

impl AllStatusHandler {
    fn seed(&self, sub: WebhookSubscription) {
        self.subscriptions
            .write()
            .expect("subscriptions write lock poisoned")
            .insert(sub.id.clone(), sub);
    }
}

impl OutboundWebhookHandler for AllStatusHandler {
    fn get_subscriptions(
        &self,
        topic: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Vec<WebhookSubscription>>> + Send>> {
        let subs = self
            .subscriptions
            .read()
            .expect("subscriptions read lock poisoned");
        let topic = topic.to_owned();
        let list: Vec<WebhookSubscription> = subs
            .values()
            .filter(|sub| sub.event_topics.iter().any(|t| t == &topic))
            .cloned()
            .collect();
        Box::pin(async move { Ok(list) })
    }

    fn log_delivery(
        &self,
        log: WebhookDeliveryLog,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        self.logs
            .write()
            .expect("logs write lock poisoned")
            .insert(log.id.clone(), log);
        Box::pin(async { Ok(()) })
    }

    fn replace_delivery_log(
        &self,
        log: WebhookDeliveryLog,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<()>> + Send>> {
        self.logs
            .write()
            .expect("logs write lock poisoned")
            .insert(log.id.clone(), log);
        Box::pin(async { Ok(()) })
    }

    fn get_subscription(
        &self,
        id: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<WebhookSubscription>>> + Send>> {
        let sub = self
            .subscriptions
            .read()
            .expect("subscriptions read lock poisoned")
            .get(id)
            .cloned();
        Box::pin(async move { Ok(sub) })
    }

    fn get_delivery_log(
        &self,
        id: &str,
    ) -> Pin<Box<dyn Future<Output = AutumnResult<Option<WebhookDeliveryLog>>> + Send>> {
        let log = self
            .logs
            .read()
            .expect("logs read lock poisoned")
            .get(id)
            .cloned();
        Box::pin(async move { Ok(log) })
    }
}

const CREATE_AUTUMN_JOBS: &str =
    include_str!("../../migrations/20260513000000_create_job_queue/up.sql");
const ADD_TRACE_CONTEXT_TO_JOBS: &str =
    include_str!("../../migrations/20260519000000_add_trace_context_to_jobs/up.sql");
const ADD_JOB_UNIQUENESS_CONCURRENCY: &str =
    include_str!("../../migrations/20260610000000_add_job_uniqueness_concurrency/up.sql");
const ADD_PENDING_UNIQUE_KEY_TO_JOBS: &str =
    include_str!("../../migrations/20260611000000_add_pending_unique_key_to_jobs/up.sql");
const ADD_QUEUE_TO_JOBS: &str =
    include_str!("../../migrations/20260628000000_add_queue_to_jobs/up.sql");

const HISTORICAL_ROWS: i64 = 250_000;

/// 250,000 historical rows (85% `completed`, 10% `failed`, 5% `enqueued`)
/// across four unrelated job names with a 30-day age spread — a busy app's
/// real `autumn_jobs` history, not an empty table. The dispatch fan-out under
/// test enqueues its own `autumn_webhook_delivery` rows on top of this.
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO autumn_jobs \
         (id, name, queue, payload, status, attempt, max_attempts, initial_backoff_ms, \
          enqueued_at, run_at, started_at, finished_at) \
         SELECT \
           'h_' || gs, \
           CASE WHEN gs % 4 = 0 THEN 'send_welcome_email' \
                WHEN gs % 4 = 1 THEN 'sync_search_index' \
                WHEN gs % 4 = 2 THEN 'process_upload' \
                ELSE 'autumn_webhook_delivery' END, \
           'default', '{{}}'::JSONB, \
           CASE WHEN gs % 100 < 85 THEN 'completed' \
                WHEN gs % 100 < 95 THEN 'failed' \
                ELSE 'enqueued' END, \
           1, 5, 1000, \
           NOW() - (random() * interval '30 days'), \
           NOW() - (random() * interval '30 days'), \
           NOW() - (random() * interval '30 days'), \
           NOW() - (random() * interval '30 days') \
         FROM generate_series(1, {HISTORICAL_ROWS}) AS gs"
    ))
    .expect("seed historical autumn_jobs history");

    conn.batch_execute("ANALYZE autumn_jobs").expect("analyze");
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

/// Prints every `autumn_jobs` `INSERT` statement from this run and returns
/// `(calls, buffers)` for the enqueue statement shape `dispatch`'s fallback
/// path issues once per active, non-disabled subscriber.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%INSERT INTO autumn_jobs%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut calls, mut buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        calls += row.calls;
        buffers += row.buffers;
    }
    println!("-- enqueue INSERT: calls={calls} buffers={buffers} --");
    (calls, buffers)
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

fn subscription(id: &str, topic: &str, status: WebhookSubscriptionStatus) -> WebhookSubscription {
    WebhookSubscription {
        id: id.to_owned(),
        target_url: format!("https://consumer-{id}.example.com/webhooks"),
        event_topics: vec![topic.to_owned()],
        secret: "whsec_ledger_fixture_signing_secret_32_bytes!".to_owned(),
        status,
        consecutive_failures: 0,
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn webhook_outbound_dispatch_fanout_profile() {
    const TOPIC: &str = "order.created";

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

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .expect("create pg_stat_statements extension");
    // Real production migration set, in order — matches CLAUDE.md's own
    // requirement that a fixture's schema can never drift from what
    // `autumn migrate` actually applies.
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

    let manager_config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager_config).build().expect("pool");

    let state = AppState::for_test()
        .with_profile("dev")
        .with_pool(pool.clone());
    let shutdown = tokio_util::sync::CancellationToken::new();
    let job_config = JobConfig {
        backend: "postgres".to_owned(),
        ..Default::default()
    };
    let job_info = JobInfo {
        version: 1,
        name: "autumn_webhook_delivery".to_owned(),
        max_attempts: 10,
        initial_backoff_ms: 1000,
        queue: "default".to_owned(),
        uniqueness: None,
        concurrency: None,
        handler: autumn_web::webhook_outbound::deliver_webhook_job,
    };
    // `run_workers = false`: this harness measures the enqueue leg only. No
    // worker claims/processes the rows, so the claim/ack machinery never
    // contaminates the `autumn_jobs` statement profile below.
    job::start_runtime(vec![job_info], &state, &shutdown, &job_config, false)
        .expect("start postgres-backed job runtime");

    // Three subscriber-fan-out sizes: a handful of integration partners, a
    // mid-size platform, and a large multi-tenant integrator. Each tier mixes
    // in a 10% share of `Disabled` subscriptions dispatch must skip without
    // costing an enqueue.
    let tiers: [(&str, i64); 3] = [("small", 25), ("mid", 250), ("large", 2_500)];

    let mut tier_results = Vec::new();
    for (label, n) in tiers {
        let handler = Arc::new(AllStatusHandler::default());
        let disabled_count = n / 10;
        let active_count = n - disabled_count;
        for i in 0..n {
            let status = if i < disabled_count {
                WebhookSubscriptionStatus::Disabled
            } else {
                WebhookSubscriptionStatus::Active
            };
            let sub = subscription(&format!("{label}_{i}"), TOPIC, status);
            handler.seed(sub);
        }

        let manager = WebhookOutboundManager::new(handler);
        let payload = serde_json::json!({
            "order_id": format!("ord_{label}"),
            "amount_cents": 4_999,
        });

        reset_stats(&mut conn);
        manager
            .dispatch(&state, TOPIC, &payload)
            .await
            .expect("dispatch");
        let (calls, buffers) = print_profile(
            &mut conn,
            &format!("{label} ({active_count} active / {disabled_count} disabled subscribers)"),
        );
        assert_eq!(
            calls, active_count,
            "one enqueue INSERT per active, non-disabled subscriber, exactly \
             (disabled subscriptions must cost zero enqueues)"
        );
        tier_results.push((label, n, active_count, calls, buffers));
    }

    println!("\n=== statement-count / buffer scaling across tiers ===");
    println!(
        "{:<6} {:>12} {:>8} {:>8} {:>10}",
        "tier", "subs", "active", "calls", "buffers"
    );
    for (label, n, active_count, calls, buffers) in &tier_results {
        println!("{label:<6} {n:>12} {active_count:>8} {calls:>8} {buffers:>10}");
    }

    // Illustrative EXPLAIN of the enqueue INSERT shape in isolation, against
    // the same seeded 250k-row history — a diagnostic, not the scale claim;
    // the scale claim is the pg_stat_statements table above.
    explain(
        &mut conn,
        "one subscriber's enqueue, issued once per dispatch recipient",
        "INSERT INTO autumn_jobs \
         (id, name, queue, payload, status, attempt, max_attempts, initial_backoff_ms, \
          enqueued_at, run_at, unique_key, unique_window, concurrency_key, concurrency_limit) \
         SELECT 'diag-explain-1', 'autumn_webhook_delivery', 'default', \
           '{}'::JSONB, 'enqueued', 1, 10, 1000, NOW(), COALESCE(NULL, NOW()), \
           NULL, NULL, NULL, NULL \
         WHERE (NULL::TEXT IS NULL OR NOT EXISTS ( \
           SELECT 1 FROM autumn_jobs dup WHERE dup.name = 'autumn_webhook_delivery' \
             AND dup.unique_key = NULL::TEXT AND dup.status IN ('enqueued', 'running') \
         )) \
         ON CONFLICT (name, unique_key) \
           WHERE unique_key IS NOT NULL AND status IN ('enqueued', 'running') DO NOTHING",
    );

    shutdown.cancel();
    job::clear_global_job_client();
}
