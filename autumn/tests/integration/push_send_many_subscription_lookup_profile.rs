//! Ledger findings harness for `WebPush::send_many`
//! (`autumn/src/push/service.rs`), the public fan-out entry point an app
//! calls to push one message to many principals at once — the shape a
//! "notify every participant in this room/thread/team" handler takes.
//!
//! `send_many` loops `principals` and calls [`WebPush::send`] once per
//! principal (`service.rs` ~371-384); `send` resolves that principal's
//! devices via `PushSubscriptionStore::list_for` (`service.rs` ~318-321).
//! Every generated/shipped backend — [`autumn_web::push::DbPushSubscriptionStore`]
//! included — issues that as its own `SELECT ... WHERE principal_id = $1`
//! round trip (`store.rs` ~819-834). So a broadcast to N principals costs N
//! sequential statements, each its own connection-pool checkout, network
//! round trip, and (per the doc comment on `send`) an empty result for a
//! principal who never subscribed still pays that whole round trip to find
//! out.
//!
//! This is a **findings issue** harness, not a before/after fix: closing it
//! means adding a batched `list_for_many` to the *public, pluggable*
//! `PushSubscriptionStore` trait every custom backend implements
//! (`store.rs` ~443-479), and the DB-backed override has to keep enforcing
//! `MAX_SUBSCRIPTIONS_PER_PRINCIPAL` **per principal**, not just overall —
//! doing that in one portable statement (Postgres + `SQLite`, per this
//! module's stated portability constraint) needs a per-group row cap
//! (a window function, or an equivalent), which is exactly the kind of
//! surface-and-correctness call the Ledger process routes to a human
//! rather than trying as the "smallest change that moves the counter".
//!
//! **Requires Docker.** CI runs it in the Docker-dependent sweep
//! (`-- --ignored`, see CLAUDE.md). Run manually with:
//!
//! ```text
//! cargo test -p autumn-web --features "db,test-support" \
//!   --test integration_tests -- --ignored push_send_many_subscription_lookup_profile \
//!   --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! A 40,000-row `push_subscriptions` table over a 30,000-principal universe
//! (`i in 1..=30_000`) — a realistic device-registration table for a
//! multi-year app:
//!
//! - Two in three principals (`i % 3 != 0`, 20,000 principals) have ever
//!   subscribed; one in three (`i % 3 == 0`, 10,000) never did — "not
//!   everyone enabled push" is real, not an edge case.
//! - Among subscribers, skewed device cardinality: 5% are "power users" with
//!   15 devices each (15,000 rows), 15% are "moderate" with 3 devices each
//!   (9,000 rows), the remaining 80% have exactly 1 device (16,000 rows).
//! - A slice of rows takes a follow-up `UPDATE` (a re-subscription event) and
//!   `ANALYZE` runs with **no** intervening `VACUUM`, so planner statistics
//!   see the dead tuples.
//!
//! `send_many` is called with three principal-id ranges standing in for a
//! small room broadcast, a team-wide announcement, and an org-wide one —
//! `1..=150`, `1..=1_500`, `1..=9_000` — each a mix of subscribed and
//! never-subscribed principals in the same 2:1 ratio as the fixture.

#![cfg(feature = "db")]

use autumn_web::push::{
    DbPushSubscriptionStore, PushMessage, RecordingPushTransport, VapidKey, WebPush,
};
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const TOTAL_PRINCIPALS: i64 = 30_000;

// Real, self-consistent RFC 8291 test key material (not secret — a fixed,
// known keypair used only so `deliver_one`'s ECDH step has a valid on-curve
// `p256dh` to operate on; delivery correctness is not what this harness
// measures, so every seeded row reuses the same pair).
const UA_PUBLIC: &str =
    "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";
const UA_AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";

/// Skewed device cardinality, 1/3 of principals never subscribed, real dead
/// tuples from a follow-up `UPDATE`+`ANALYZE` — the fixture shape the Ledger
/// process requires (real row counts, real cardinality skew, real
/// dead-tuple ratio).
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO push_subscriptions \
         (principal_id, endpoint, p256dh, auth, created_at) \
         SELECT \
           p.principal::text, \
           'https://push.example.com/' || p.principal || '/' || d.device, \
           '{UA_PUBLIC}', \
           '{UA_AUTH}', \
           TIMESTAMP '2024-01-01 00:00:00' + (p.principal || ' minutes')::interval \
         FROM generate_series(1, {TOTAL_PRINCIPALS}) AS p(principal) \
         CROSS JOIN LATERAL generate_series(1, \
           CASE \
             WHEN p.principal % 3 = 0 THEN 0 \
             WHEN p.principal % 100 < 5 THEN 15 \
             WHEN p.principal % 100 < 20 THEN 3 \
             ELSE 1 \
           END \
         ) AS d(device) \
         WHERE p.principal % 3 != 0"
    ))
    .expect("seed push_subscriptions");

    // Real dead tuples: a re-subscription event touches every "moderate"
    // principal's first device (rewrites the row rather than inserting a
    // new one, exactly like `DbPushSubscriptionStore::save`'s
    // `ON CONFLICT (endpoint) DO UPDATE`).
    conn.batch_execute(&format!(
        "UPDATE push_subscriptions SET auth = '{UA_AUTH}' \
         WHERE principal_id::bigint % 3 != 0 \
           AND principal_id::bigint % 100 >= 5 AND principal_id::bigint % 100 < 20 \
           AND endpoint LIKE '%/1'"
    ))
    .expect("create dead tuples");
    conn.batch_execute("ANALYZE push_subscriptions")
        .expect("analyze");
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

/// Prints every `push_subscriptions` statement from this run and returns
/// `(calls, buffers)` totals for the lookup statement shape.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%push_subscriptions%' \
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
        if normalized.starts_with("SELECT") {
            calls += row.calls;
            buffers += row.buffers;
        }
    }
    println!("-- lookup SELECT: calls={calls} buffers={buffers} --");
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn push_send_many_subscription_lookup_profile() {
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
    // Schema matches exactly what `autumn generate pwa` scaffolds
    // (`autumn-cli/src/generate/pwa.rs::push_subscriptions_up_sql`) —
    // including the `principal_id` index the generator's own test
    // (`push_migration_indexes_principal_id_for_the_send_path`) requires,
    // since every send does `WHERE principal_id = …` and a harness that
    // omitted it would be measuring a schema no deployment actually runs.
    conn.batch_execute(
        "CREATE TABLE push_subscriptions ( \
            id BIGSERIAL PRIMARY KEY, \
            principal_id TEXT NOT NULL, \
            endpoint TEXT NOT NULL UNIQUE, \
            p256dh TEXT NOT NULL, \
            auth TEXT NOT NULL, \
            created_at TIMESTAMPTZ NOT NULL \
         )",
    )
    .expect("create push_subscriptions");
    conn.batch_execute(
        "CREATE INDEX push_subscriptions_principal_id_idx \
         ON push_subscriptions (principal_id)",
    )
    .expect("create principal_id index");

    seed_fixture(&mut conn);

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(config).build().expect("pool");
    let store = DbPushSubscriptionStore::new(pool);
    let push = WebPush::new(
        store,
        VapidKey::generate(),
        "mailto:ops@example.com",
        RecordingPushTransport::new(),
    );

    // Three broadcast sizes standing in for a room, a team, and an org —
    // each principal range keeps the fixture's 2:1 subscribed:unsubscribed
    // ratio (every `i % 3 == 0` principal never subscribed).
    let tiers: [(&str, i64); 3] = [("room", 150), ("team", 1_500), ("org", 9_000)];

    let mut tier_results = Vec::new();
    for (label, n) in tiers {
        let principals: Vec<i64> = (1..=n).collect();
        let expected_calls = n; // one `list_for` round trip per principal, always
        reset_stats(&mut conn);
        let report = push
            .send_many(principals, &PushMessage::new("Deploy", "shipped"))
            .await
            .expect("send_many");
        let (calls, buffers) = print_profile(&mut conn, &format!("{label} ({n} principals)"));
        assert_eq!(
            calls, expected_calls,
            "one lookup call per principal, exactly, regardless of hit/miss"
        );
        // Sanity: at least one subscribed principal in range attempted a
        // delivery (dead or not) — this harness measures lookup cost, not
        // delivery outcome, so the report is only checked for "did anything
        // happen", not exact counts.
        let subscribed = n - n / 3;
        assert!(
            subscribed == 0 || report.delivered + report.failed > 0 || !report.pruned.is_empty(),
            "{label}: at least one delivery must have been attempted"
        );
        tier_results.push((label, n, calls, buffers));
    }

    println!("\n=== statement-count / buffer scaling across tiers ===");
    println!(
        "{:<6} {:>12} {:>12} {:>14}",
        "tier", "principals", "calls", "buffers"
    );
    for (label, n, calls, buffers) in &tier_results {
        println!("{label:<6} {n:>12} {calls:>12} {buffers:>14}");
    }

    // Illustrative EXPLAIN at the largest tier — a diagnostic, not the scale
    // claim; the scale claim is the pg_stat_statements table above.
    explain(
        &mut conn,
        "one principal's lookup, issued once per broadcast recipient",
        "SELECT * FROM push_subscriptions WHERE principal_id = '1' ORDER BY id ASC LIMIT 20",
    );
}
