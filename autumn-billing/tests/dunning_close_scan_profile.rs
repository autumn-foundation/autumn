//! Ledger findings/fix harness for `close_dunning_for`
//! (`autumn-billing/src/reconcile.rs`), the step a `customer.subscription.deleted`
//! (or any subscription-changed-to-`canceled`) webhook takes to close out any
//! dunning schedule still open for the subscription that just ended.
//!
//! Drives the REAL production path: `reconcile::apply` (`reconcile.rs`) is
//! the function both the `POST /billing/webhook` route and the plugin's own
//! `apply_event` test helper call; this harness calls it directly with a
//! `SubscriptionDeleted` event, the same way `routes.rs`'s webhook receiver
//! does after `StripeProvider::parse_event` decodes a real payload — this
//! harness only skips the HTTP transport and signature step, which is
//! orthogonal to the query this harness measures.
//!
//! Before the fix in this same PR, `close_dunning_for` calls
//! `BillingStore::open_dunning()` — every `Pending`/`Running` row in the
//! **entire** `billing_dunning` table, system-wide, ordered by
//! `next_attempt_at` — and then filters the returned `Vec` in Rust for the
//! one `subscription_id` it actually wants (reconcile.rs:274, `for row in
//! store.open_dunning().await? { if row.subscription_id.as_deref() !=
//! Some(subscription_id) { continue; } ... }`). The only index on
//! `billing_dunning` is `billing_dunning_state_idx (state, next_attempt_at)`
//! — no leading column on `subscription_id` — so this is a full index-range
//! scan (and matching heap fetch) of every open dunning row in the system on
//! *every* subscription cancellation, discarding all but the ~0-1 rows that
//! belong to the canceling subscription. A payment-provider bulk-cancel
//! sweep, or just normal `SaaS` churn at scale, pays for the whole open-dunning
//! backlog on every single cancellation.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p autumn-billing --test dunning_close_scan_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! This crate has no consolidated Docker sweep (unlike `autumn`/`autumn-cli`,
//! see CLAUDE.md) — `mirror_db` is the other Docker-backed suite here and is
//! invoked by an explicit `--test mirror_db` line in `.github/workflows/ci.yml`'s
//! "Run Docker-dependent tests" step (and again in the coverage step), so this
//! binary needs, and has, the same explicit `--test dunning_close_scan_profile
//! -- --ignored` line right next to it in both places.
//!
//! ## Fixture
//!
//! 20,000 customers, each with one subscription (a realistic mid-size `SaaS`
//! tenant base). One in five subscriptions (`gs % 5 == 0`, 4,000 rows) has an
//! **open** (`pending`/`running`) dunning row — the fraction of a customer
//! base with a currently-failing card is real, not universal. One in seven
//! (`gs % 7 == 0`, ~2,857 rows, a disjoint invoice-id namespace so it never
//! collides with the open set's primary key) has a **closed** historical
//! dunning row (`recovered`/`exhausted`) — the table's real long-tail size
//! includes rows this query's `state` predicate already excludes, so seeding
//! them tests that the fix's new index doesn't accidentally widen what it
//! matches. A follow-up `UPDATE` touches every closed row and `ANALYZE` runs
//! with **no** intervening `VACUUM`, so planner statistics see real dead
//! tuples, matching every other Ledger fixture in this repo.
//!
//! 296 subscriptions are then canceled via `reconcile::apply`: 148 chosen
//! from the "has an open row" set (`gs = 135, 270, …, 19980`) and 148 from a
//! disjoint "never had a dunning row at all" set (`gs = 6, 41, 76, …`, every
//! value `% 35` congruent to 6 — never a multiple of `DUNNING_STRIDE` (5) or
//! `CLOSED_STRIDE` (7), so it has neither an open nor a closed row) — a
//! mixed cancellation workload where most cancellations are NOT
//! payment-related (the realistic case) and a minority must actually settle
//! an open row (also realistic, and the case that proves the fix doesn't
//! just always return empty).

// Row/statement counts here top out in the low hundred-thousands, nowhere
// near f64's 52-bit mantissa limit -- every `as f64` below is a display
// ratio, never a value compared against anything, so the lossy cast is
// harmless. The harness's single `#[tokio::test]` body is long because it is
// the linear seed -> fire -> profile -> verify pipeline this Ledger process
// requires in one place, not because it is doing several unrelated things.
#![allow(clippy::cast_precision_loss, clippy::too_many_lines)]

#[path = "cases/support.rs"]
mod support;

use std::sync::Arc;

use autumn_billing::{
    BillingEvent, BillingEventKind, BillingService, DbBillingStore, NoHooks, SubscriptionSnapshot,
    SubscriptionStatus,
};
use autumn_web::reexports::diesel_migrations::MigrationHarness;
use autumn_web::test::TestApp;
use chrono::{TimeZone, Utc};
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const TOTAL_SUBSCRIPTIONS: i64 = 20_000;
/// Every 5th subscription has ever had a failed invoice (an open dunning row).
const DUNNING_STRIDE: i64 = 5;
/// Every 7th subscription also has a closed historical dunning row (a
/// disjoint invoice-id namespace, so it never collides with the open set).
const CLOSED_STRIDE: i64 = 7;
/// `135 = DUNNING_STRIDE * 27`: every one of these `gs` values is also a
/// multiple of `DUNNING_STRIDE`, so it lands in the "has an open row" set.
const EVENT_STRIDE: i64 = 135;
/// Number of cancellations fired from each of the two disjoint groups.
const EVENT_GROUPS: i64 = 148;

/// 20,000 customers/subscriptions (1:1), 4,000 open dunning rows (one in
/// five subscriptions), ~2,857 closed historical dunning rows (one in
/// seven, disjoint invoice-id namespace) with real dead tuples from a
/// follow-up `UPDATE`+`ANALYZE` and no `VACUUM` — the fixture shape the
/// Ledger process requires (real row counts, real cardinality, real
/// dead-tuple ratio).
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
         SELECT 'inv_' || gs, 'cust_' || gs, 'sub_' || gs, \
           1, \
           TIMESTAMP '2025-06-02 00:00:00' + (gs || ' minutes')::interval, \
           CASE WHEN gs % 10 = 0 THEN 'running' ELSE 'pending' END, \
           TIMESTAMP '2025-06-01 12:00:00' \
         FROM generate_series(1, {TOTAL_SUBSCRIPTIONS}) AS gs \
         WHERE gs % {DUNNING_STRIDE} = 0"
    ))
    .expect("seed open billing_dunning rows");

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

    // Real dead tuples on the closed rows (never touches the open rows this
    // harness measures, so it can't shift next_attempt_at ordering there).
    conn.batch_execute(
        "UPDATE billing_dunning SET updated_at = NOW() \
         WHERE state IN ('recovered', 'exhausted')",
    )
    .expect("create dead tuples");
    conn.batch_execute("ANALYZE billing_customers")
        .expect("analyze customers");
    conn.batch_execute("ANALYZE billing_subscriptions")
        .expect("analyze subscriptions");
    conn.batch_execute("ANALYZE billing_dunning")
        .expect("analyze dunning");
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

/// `gs` values `EVENT_STRIDE, 2*EVENT_STRIDE, …` up to `TOTAL_SUBSCRIPTIONS`
/// (`EVENT_GROUPS` of them) — every one a multiple of `DUNNING_STRIDE`, so
/// each has exactly one open row today.
fn with_dunning_group() -> Vec<i64> {
    (1..=EVENT_GROUPS).map(|k| k * EVENT_STRIDE).collect()
}

/// `gs = 35*k + 6` (`35 = DUNNING_STRIDE * CLOSED_STRIDE`, so this residue is
/// periodic in both). `6 % 5 == 1` (never opens a dunning row) and
/// `6 % 7 == 6` (never gets a closed one either) — every value in the
/// sequence keeps both properties, so this group has zero dunning rows,
/// open or closed. (An earlier version used `gs % 5 == 1` alone, which
/// looked disjoint from the open set but wasn't disjoint from the closed
/// set: `gs % 5 == 1 AND gs % 7 == 0` is satisfiable — `gs = 21` is the
/// smallest example — so roughly one in seven of those `gs` still had a
/// closed row. Caught by the harness's own `without_dunning_any == 0`
/// assertion during the baseline run, not assumed away.)
fn without_dunning_group() -> Vec<i64> {
    (0..EVENT_GROUPS).map(|k| 35 * k + 6).collect()
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

/// Prints every statement this run issued, ranked by buffers, then by
/// calls. Returns `(calls, buffers)` for the `billing_dunning` scan this
/// harness targets — a `SELECT` naming `billing_dunning` whose text does
/// NOT also contain `INSERT`/`UPDATE`/`DELETE` (isolates the read from the
/// `settle_dunning` write, which is its own statement) — and the workload's
/// grand total buffers across every statement, for the profile percentage.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} (by buffers) ===");
    let by_buffers = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY buffers DESC LIMIT 10",
    )
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
    let by_calls = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC LIMIT 10",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements by calls");
    for row in &by_calls {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<10} {normalized}",
            row.calls, row.buffers
        );
    }

    // The two LIMIT 10 lists above are for the printed report only; the two
    // totals below query the full, untruncated statement set.
    let grand_total_all: i64 = diesel::sql_query(
        "SELECT COALESCE(SUM(shared_blks_hit + shared_blks_read), 0)::bigint AS n \
         FROM pg_stat_statements WHERE query NOT ILIKE '%pg_stat_statements%'",
    )
    .get_result::<CountRow>(conn)
    .expect("grand total buffers")
    .n;

    // Queried directly (not from the truncated top-10 lists above) so this
    // number is exact regardless of where the statement ranks. A leading
    // `SELECT` anchor already rules out the `UPDATE billing_dunning SET ...`
    // statement (`settle_dunning`) without also excluding text anywhere in
    // the string: that statement starts with `UPDATE`, not `SELECT`. (An
    // earlier version excluded `%UPDATE%` anywhere in the text instead,
    // which excluded every row here too -- every one of these SELECTs names
    // the `updated_at` column, and `"updated_at"` contains the substring
    // `UPDATE` in its first six letters.)
    let target_rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE 'SELECT%billing_dunning%'",
    )
    .load::<StatementRow>(conn)
    .expect("query the target statement");
    assert!(
        target_rows.len() <= 1,
        "expected at most one open-dunning-read statement shape, found {}: {target_rows:?}",
        target_rows.len()
    );
    let (target_calls, target_buffers) =
        target_rows.first().map_or((0, 0), |r| (r.calls, r.buffers));
    println!(
        "\n-- target SELECT: calls={target_calls} buffers={target_buffers} \
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

#[derive(QueryableByName, Debug)]
struct DunningStateRow {
    #[diesel(sql_type = Text)]
    invoice_id: String,
    #[diesel(sql_type = Text)]
    state: String,
}

/// Final `(invoice_id, state)` for exactly the open rows this run should
/// have settled, sorted by `invoice_id` (deterministic, no ties) — the
/// result-equivalence artifact compared byte-for-byte between the baseline
/// (pre-fix) and after (post-fix) commits.
fn dump_settled_state(conn: &mut PgConnection, with_dunning: &[i64]) -> String {
    use diesel::RunQueryDsl;
    let ids = with_dunning
        .iter()
        .map(|gs| format!("'inv_{gs}'"))
        .collect::<Vec<_>>()
        .join(",");
    let rows = diesel::sql_query(format!(
        "SELECT invoice_id, state FROM billing_dunning WHERE invoice_id IN ({ids}) \
         ORDER BY invoice_id"
    ))
    .load::<DunningStateRow>(conn)
    .expect("dump settled state");
    rows.into_iter()
        .map(|r| format!("{}:{}", r.invoice_id, r.state))
        .collect::<Vec<_>>()
        .join(",")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker (testcontainers)"]
async fn dunning_close_scan_profile() {
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

    seed_fixture(&mut conn);

    let with_dunning = with_dunning_group();
    let without_dunning = without_dunning_group();

    let open_total = count(
        &mut conn,
        "SELECT COUNT(*) AS n FROM billing_dunning WHERE state IN ('pending', 'running')",
    );
    assert_eq!(
        open_total,
        TOTAL_SUBSCRIPTIONS / DUNNING_STRIDE,
        "seeded open-row count"
    );

    let with_dunning_ids = with_dunning
        .iter()
        .map(|gs| format!("'sub_{gs}'"))
        .collect::<Vec<_>>()
        .join(",");
    let with_dunning_open = count(
        &mut conn,
        &format!(
            "SELECT COUNT(*) AS n FROM billing_dunning \
             WHERE subscription_id IN ({with_dunning_ids}) AND state IN ('pending', 'running')"
        ),
    );
    assert_eq!(
        with_dunning_open, EVENT_GROUPS,
        "every subscription in the 'has dunning' cancellation group must have exactly one open row"
    );

    let without_dunning_ids = without_dunning
        .iter()
        .map(|gs| format!("'sub_{gs}'"))
        .collect::<Vec<_>>()
        .join(",");
    let without_dunning_any = count(
        &mut conn,
        &format!(
            "SELECT COUNT(*) AS n FROM billing_dunning WHERE subscription_id IN ({without_dunning_ids})"
        ),
    );
    assert_eq!(
        without_dunning_any, 0,
        "the 'never had dunning' cancellation group must have zero dunning rows, open or closed"
    );

    println!(
        "\n-- fixture: {TOTAL_SUBSCRIPTIONS} subscriptions, {open_total} open dunning rows \
         system-wide; canceling {EVENT_GROUPS} subscriptions with an open row \
         + {EVENT_GROUPS} with none --"
    );

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(config).build().expect("pool");
    let store: Arc<dyn autumn_billing::BillingStore> = Arc::new(DbBillingStore::new(pool));
    let provider = support::FakeProvider::new();
    let service = BillingService::new(
        provider,
        store,
        support::catalog(),
        support::config(),
        Arc::new(NoHooks),
    );

    // A bare test app: only its `AppState` (clock/entropy) is used.
    // `reconcile::apply` takes the `BillingService` above as a parameter, so
    // no plugin needs to be mounted on this app at all.
    let client = TestApp::new().build();
    let state = client.state();

    let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

    reset_stats(&mut conn);

    let mut applied = 0usize;
    for (i, gs) in with_dunning
        .iter()
        .chain(without_dunning.iter())
        .enumerate()
    {
        let event = BillingEvent::new(
            format!("evt_cancel_{gs}"),
            base + chrono::Duration::seconds(i64::try_from(i).expect("fits in i64")),
            BillingEventKind::SubscriptionDeleted(SubscriptionSnapshot::new(
                format!("sub_stripe_{gs}"),
                format!("cus_stripe_{gs}"),
                SubscriptionStatus::Canceled,
            )),
        );
        autumn_billing::reconcile::apply(state, &service, event)
            .await
            .expect("reconcile a subscription-deleted event");
        applied += 1;
    }
    assert_eq!(
        applied,
        usize::try_from(EVENT_GROUPS * 2).expect("fits in usize"),
        "every cancellation applied"
    );

    let (target_calls, target_buffers, workload_buffers) =
        print_profile(&mut conn, "296 subscription cancellations");

    let buffers_pct = 100.0 * target_buffers as f64 / workload_buffers.max(1) as f64;
    println!(
        "\n-- close_dunning_for's open-row scan: calls={target_calls} buffers={target_buffers} \
         ({buffers_pct:.1}% of {workload_buffers} total workload buffers) --"
    );
    assert_eq!(
        target_calls,
        EVENT_GROUPS * 2,
        "close_dunning_for issues exactly one open-dunning read per cancellation, \
         whether or not that subscription has anything open"
    );
    // Impact floor: a system-wide scan of every open row (pre-fix) costs
    // 122 buffers per call regardless of the fixture's cardinality (a Seq
    // Scan reads every pending/running row every time); a subscription-
    // scoped index lookup costs O(1-2) per call. 10 buffers/call leaves
    // generous headroom above the fixed post-fix cost while staying far
    // below the pre-fix cost at ANY fixture size -- this is a scan-shape
    // difference, not a threshold tuned to this fixture's row count.
    assert!(
        target_buffers < 10 * (EVENT_GROUPS * 2),
        "open-dunning read must be a per-subscription lookup, not a table-wide scan: \
         buffers={target_buffers} calls={target_calls}"
    );

    let state_dump = dump_settled_state(&mut conn, &with_dunning);
    println!("\n=== settled state (invoice_id:state, sorted by invoice_id) ===");
    println!("{state_dump}");
    let still_open = state_dump
        .split(',')
        .filter(|s| s.ends_with(":pending") || s.ends_with(":running"))
        .count();
    assert_eq!(
        still_open, 0,
        "every open row of a canceled subscription must be settled"
    );
    let canceled_count = state_dump
        .split(',')
        .filter(|s| s.ends_with(":canceled"))
        .count();
    assert_eq!(
        canceled_count,
        usize::try_from(EVENT_GROUPS).expect("fits in usize"),
        "exactly the seeded open rows transition to canceled, none extra"
    );

    explain(
        &mut conn,
        "before: system-wide open-row scan (open_dunning, pre-fix)",
        "SELECT invoice_id, customer_id, subscription_id, attempt, next_attempt_at, state, updated_at \
         FROM billing_dunning \
         WHERE state IN ('pending', 'running') \
         ORDER BY next_attempt_at ASC, invoice_id ASC",
    );
    explain(
        &mut conn,
        "after: subscription-scoped lookup (open_dunning_for_subscription, this PR)",
        "SELECT invoice_id, customer_id, subscription_id, attempt, next_attempt_at, state, updated_at \
         FROM billing_dunning \
         WHERE subscription_id = 'sub_10' AND state IN ('pending', 'running') \
         ORDER BY next_attempt_at ASC, invoice_id ASC",
    );
}
