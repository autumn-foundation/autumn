//! Ledger findings/fix harness for the generated `ledger_as_of`/`ledger_diff`
//! read path (`autumn-macros/src/repository.rs`), driven through the real
//! public API a compliance/support audit calls: "what did this record look
//! like at instant X".
//!
//! # The mechanism
//!
//! `ledger_as_of`/`ledger_diff` are pure functions
//! ([`snapshot_as_of`](autumn_web::ledger::snapshot_as_of)) over whatever
//! `ledger_revisions(record_id)` returns — and `ledger_revisions` has exactly
//! one SQL shape: `SELECT ... FROM _autumn_ledger_revisions WHERE table_name =
//! $1 AND record_id = $2 AND (tenant) ORDER BY seq ASC`, no `LIMIT`, no bound
//! on `as_of` at all. It reads **every** stored revision of the record — every
//! row, every `snapshot` TEXT column, the largest column in the table — to
//! answer a question with exactly one answer, regardless of whether `as_of`
//! asks about the current instant or the record's very first revision.
//!
//! For a `#[repository(ledgered = true)]` entity that is exactly the kind of
//! record the feature exists for: a financial account, an invoice, a
//! contract — something adjusted repeatedly over a long operational life and
//! later audited. A hot account with a few thousand postings against it pays
//! for a few-thousand-row, full-snapshot read on every single as-of/diff call,
//! even when the auditor is asking "what did it look like an hour ago".
//!
//! # Fixture
//!
//! Three "hot" ledgered accounts at three chain depths (300 / 700 / 1,200
//! revisions — real operational history, not a single huge outlier) plus a
//! long tail of 150 accounts touched only 6 times each (skewed cardinality,
//! the same 80/20 shape the export/reaper Ledger fixtures use, now expressed
//! as chain depth rather than row count). Every revision is written through
//! the real `save`/`update` write path — the actual `#[repository]`-generated
//! append, hash chain and high-water bookkeeping — so the fixture is exactly
//! what production writes, not a synthetic bulk insert. The live row for each
//! hot account is itself updated hundreds of times, so its heap carries real
//! dead tuples by the time `ANALYZE` runs (no `VACUUM`), same as the other
//! Ledger fixtures' dead-tuple requirement.
//!
//! **Requires Docker.** CI runs it in the Docker-dependent sweep (`--
//! --ignored`, see CLAUDE.md). Run manually with:
//!
//! ```text
//! cargo test -p autumn-web --features "test-support" \
//!   --test integration_tests -- --ignored ledger_as_of_deep_chain_profile \
//!   --nocapture --test-threads=1
//! ```

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]
#![allow(clippy::cast_possible_wrap)]

use autumn_web::current::with_actor;
use autumn_web::hooks::Patch;
use chrono::{DateTime, Utc};
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, SimpleAsyncConnection as _};
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    ledger_deep_chain_accounts (id) {
        id -> Int8,
        external_ref -> Text,
        balance_cents -> Int8,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "ledger_deep_chain_accounts")]
pub struct LedgerDeepChainAccount {
    #[id]
    pub id: i64,
    pub external_ref: String,
    pub balance_cents: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    LedgerDeepChainAccount,
    table = "ledger_deep_chain_accounts",
    soft_delete,
    ledgered = true
)]
pub trait LedgerDeepChainAccountRepository {}

/// The migration SQL Autumn actually ships, applied verbatim — same
/// convention as `ledger_postgres.rs`.
const LEDGER_UP: &str =
    include_str!("../../version_history_migrations/20260826000000_create_ledger_revisions/up.sql");
const LEDGER_HIGH_WATER_UP: &str =
    include_str!("../../version_history_migrations/20260901213107_create_ledger_high_water/up.sql");
const VERSION_HISTORY_UP: &str =
    include_str!("../../version_history_migrations/20260526000000_create_version_history/up.sql");

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgLedgerDeepChainAccountRepository {
    PgLedgerDeepChainAccountRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

async fn setup() -> (
    Pool<AsyncPgConnection>,
    String,
    testcontainers::ContainerAsync<Postgres>,
) {
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

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(10).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .await
        .expect("create pg_stat_statements extension");
    for ddl in [
        "CREATE TABLE IF NOT EXISTS ledger_deep_chain_accounts (
             id BIGSERIAL PRIMARY KEY,
             external_ref TEXT NOT NULL,
             balance_cents BIGINT NOT NULL,
             deleted_at TIMESTAMP
         )",
        VERSION_HISTORY_UP,
        LEDGER_UP,
        LEDGER_HIGH_WATER_UP,
    ] {
        conn.batch_execute(ddl)
            .await
            .unwrap_or_else(|err| panic!("apply DDL: {err}\n{ddl}"));
    }

    (pool, url, container)
}

/// Writes one hot account's chain through the real `save`/`update` path:
/// `depth` total revisions (1 insert + `depth - 1` updates), each committed
/// before the next is issued (a chain is inherently sequential — each append
/// reads the previous head). Returns the record id and, for every revision,
/// a `Utc::now()` sampled immediately after that write's `await` returned —
/// strictly after the database committed that revision and strictly before
/// the next one starts, so `oracle[k]` is a safe "as of" instant for "the
/// state after revision k+1" (`oracle.len() == depth`).
async fn seed_hot_chain(
    pool: Pool<AsyncPgConnection>,
    external_ref: &str,
    depth: usize,
) -> (i64, Vec<DateTime<Utc>>) {
    let repo = build_repo(pool);
    let created = with_actor("system", async {
        repo.save(&NewLedgerDeepChainAccount {
            external_ref: external_ref.to_string(),
            balance_cents: 0,
        })
        .await
        .expect("insert hot account")
    })
    .await;
    let id = created.id;
    let mut oracle = Vec::with_capacity(depth);
    oracle.push(Utc::now());
    for i in 1..depth {
        with_actor("system", async {
            repo.update(
                id,
                &UpdateLedgerDeepChainAccount {
                    balance_cents: Patch::Set(i as i64),
                    ..Default::default()
                },
            )
            .await
            .expect("update hot account")
        })
        .await;
        oracle.push(Utc::now());
    }
    (id, oracle)
}

/// A long tail of accounts touched only `depth_each` times each — the same
/// skewed cardinality shape the export/reaper Ledger fixtures use, so the
/// `_autumn_ledger_revisions` table looks like a real deployment's rather
/// than holding only the three hot chains under test.
async fn seed_long_tail(pool: Pool<AsyncPgConnection>, count: usize, depth_each: usize) {
    let repo = build_repo(pool);
    for n in 0..count {
        let created = with_actor("system", async {
            repo.save(&NewLedgerDeepChainAccount {
                external_ref: format!("tail-{n}"),
                balance_cents: 0,
            })
            .await
            .expect("insert tail account")
        })
        .await;
        let id = created.id;
        for i in 1..depth_each {
            with_actor("system", async {
                repo.update(
                    id,
                    &UpdateLedgerDeepChainAccount {
                        balance_cents: Patch::Set(i as i64),
                        ..Default::default()
                    },
                )
                .await
                .expect("update tail account")
            })
            .await;
        }
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

/// Prints every `_autumn_ledger_revisions` statement from this run and
/// returns `(calls, buffers)` totals for the two statement SHAPES the
/// generated code can issue: the unbounded `ledger_revisions` scan
/// (`ORDER BY seq ASC`, no `LIMIT`) and the bounded `as_of` lookup (`ORDER BY
/// seq DESC LIMIT`). Whichever shape is compiled in shows up under the
/// matching bucket, so this same harness measures both the pre-fix and
/// post-fix implementation, unchanged.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%_autumn_ledger_revisions%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut unbounded_calls, mut unbounded_buffers) = (0i64, 0i64);
    let (mut bounded_calls, mut bounded_buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        if normalized.contains("ORDER BY seq DESC") {
            bounded_calls += row.calls;
            bounded_buffers += row.buffers;
        } else if normalized.contains("ORDER BY seq ASC") {
            unbounded_calls += row.calls;
            unbounded_buffers += row.buffers;
        }
    }
    println!(
        "-- unbounded (ledger_revisions) shape: calls={unbounded_calls} buffers={unbounded_buffers} \
         -- bounded (as-of) shape: calls={bounded_calls} buffers={bounded_buffers} --"
    );
    (
        unbounded_calls,
        unbounded_buffers,
        bounded_calls,
        bounded_buffers,
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
async fn ledger_as_of_deep_chain_profile() {
    let (pool, url, _container) = setup().await;

    // Three chain depths -- the Ledger process's "admissible at >= 3 sizes"
    // bar for a plan-shape/row-count claim -- written concurrently (each
    // chain is internally sequential; the three chains have no shared state)
    // plus a skewed long tail, all through the real write path.
    let ((hot_300, oracle_300), (hot_700, oracle_700), (hot_1200, oracle_1200), ()) = tokio::join!(
        seed_hot_chain(pool.clone(), "hot-300", 300),
        seed_hot_chain(pool.clone(), "hot-700", 700),
        seed_hot_chain(pool.clone(), "hot-1200", 1200),
        seed_long_tail(pool.clone(), 150, 6),
    );

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    // Real dead tuples: each hot account's live row was updated hundreds of
    // times above; ANALYZE without an intervening VACUUM leaves the planner
    // seeing them, same as the other Ledger fixtures' dead-tuple requirement.
    conn.batch_execute("ANALYZE ledger_deep_chain_accounts, _autumn_ledger_revisions")
        .expect("analyze");

    let repo = build_repo(pool);
    let chains: [(&str, i64, &[DateTime<Utc>]); 3] = [
        ("hot-300", hot_300, &oracle_300),
        ("hot-700", hot_700, &oracle_700),
        ("hot-1200", hot_1200, &oracle_1200),
    ];

    // For each depth, ask a realistic near-head audit question -- "what did
    // this look like 5 postings ago" -- and confirm the answer is correct
    // (equivalence: same reconstructed value regardless of which query shape
    // answered it) while the statement/buffer profile is captured.
    println!("\n=== near-head as-of: statement/buffer scaling across chain depths ===");
    println!(
        "{:<10} {:>8} {:>16} {:>18} {:>16} {:>18}",
        "chain",
        "depth",
        "unbounded calls",
        "unbounded buffers",
        "bounded calls",
        "bounded buffers"
    );
    for (label, id, oracle) in &chains {
        let depth = oracle.len();
        let near_head_pos = depth - 6; // "5 postings before the current head"
        let as_of = oracle[near_head_pos];
        reset_stats(&mut conn);
        let reconstructed = repo
            .ledger_as_of(*id, as_of)
            .await
            .expect("as-of read")
            .expect("record existed at this instant");
        assert_eq!(
            reconstructed.balance_cents,
            i64::try_from(near_head_pos).expect("position fits in i64"),
            "{label}: near-head as-of must reconstruct the exact revision in force"
        );
        let (u_calls, u_buffers, b_calls, b_buffers) = print_profile(
            &mut conn,
            &format!("{label} (depth {depth}) near-head as-of"),
        );
        println!(
            "{label:<10} {depth:>8} {u_calls:>16} {u_buffers:>18} {b_calls:>16} {b_buffers:>18}"
        );
    }

    // Worst case, disclosed: asking about the record's very FIRST revision
    // still has to walk back through the whole chain under either shape,
    // because `valid_from`/`recorded_at` bounds alone give no guarantee the
    // qualifying row sits near the head. This must not regress -- the bounded
    // shape should read no more than the unbounded one ever did.
    reset_stats(&mut conn);
    let earliest = repo
        .ledger_as_of(hot_1200, oracle_1200[0])
        .await
        .expect("as-of read")
        .expect("record existed at its first revision");
    assert_eq!(
        earliest.balance_cents, 0,
        "the first revision is the insert"
    );
    let (_, _, worst_case_bounded_calls, worst_case_bounded_buffers) = print_profile(
        &mut conn,
        "hot-1200 worst case: as-of at the FIRST revision",
    );

    // A field-level diff between two RECENT instants -- the other generated
    // caller of the bounded lookup. Both endpoints are resolved from one
    // UNION ALL statement on one connection (not two separate calls), so a
    // write landing between them can't resolve `from`/`to` against two
    // different database states -- the same snapshot guarantee the old
    // single `ledger_revisions` read had. Statement count stays at 1;
    // buffers/rows read fall sharply -- print-only, not asserted, so the
    // same harness also captures the pre-fix shape cleanly.
    reset_stats(&mut conn);
    let recent_from = oracle_1200[oracle_1200.len() - 11];
    let recent_to = oracle_1200[oracle_1200.len() - 1];
    let diff = repo
        .ledger_diff(hot_1200, recent_from, recent_to)
        .await
        .expect("diff");
    assert_eq!(diff.changes.len(), 1, "{:?}", diff.changes);
    assert_eq!(diff.changes[0].column, "balance_cents");
    print_profile(
        &mut conn,
        "hot-1200 ledger_diff across the last 10 postings",
    );

    println!(
        "\n-- worst-case (first revision) bounded shape: calls={worst_case_bounded_calls} \
         buffers={worst_case_bounded_buffers} --"
    );

    // Illustrative EXPLAIN of the exact statement `ledger_revisions`/the
    // as-of lookup issues, at the deepest chain, near head vs record start --
    // the scale claim is the pg_stat_statements table above; this is the plan
    // shape and the `actual rows` at the scan node behind it.
    explain(
        &mut conn,
        "unbounded ledger_revisions shape -- what ledger_as_of currently reads regardless of `as_of` \
         (hot-1200)",
        &format!(
            "SELECT id, table_name, tenant_id, record_id, seq, op, actor, request_id, snapshot, \
             valid_from, recorded_at, prev_hash, hash \
             FROM _autumn_ledger_revisions \
             WHERE table_name = 'ledger_deep_chain_accounts' AND record_id = {hot_1200} \
             AND (NULL::text IS NULL OR tenant_id = NULL) \
             ORDER BY seq ASC"
        ),
    );
    explain(
        &mut conn,
        &format!(
            "bounded as-of shape, near head -- 'what did it look like 5 postings ago' (hot-1200, \
             at={})",
            oracle_1200[oracle_1200.len() - 6]
        ),
        &format!(
            "SELECT id, table_name, tenant_id, record_id, seq, op, actor, request_id, snapshot, \
             valid_from, recorded_at, prev_hash, hash \
             FROM _autumn_ledger_revisions \
             WHERE table_name = 'ledger_deep_chain_accounts' AND record_id = {hot_1200} \
             AND (NULL::text IS NULL OR tenant_id = NULL) \
             AND ('{}'::timestamptz IS NULL OR recorded_at <= '{}'::timestamptz) \
             AND (NULL::timestamptz IS NULL OR valid_from <= NULL) \
             ORDER BY seq DESC LIMIT 1",
            oracle_1200[oracle_1200.len() - 6].to_rfc3339(),
            oracle_1200[oracle_1200.len() - 6].to_rfc3339(),
        ),
    );
    explain(
        &mut conn,
        &format!(
            "bounded as-of shape, worst case -- record's first revision (hot-1200, at={})",
            oracle_1200[0]
        ),
        &format!(
            "SELECT id, table_name, tenant_id, record_id, seq, op, actor, request_id, snapshot, \
             valid_from, recorded_at, prev_hash, hash \
             FROM _autumn_ledger_revisions \
             WHERE table_name = 'ledger_deep_chain_accounts' AND record_id = {hot_1200} \
             AND (NULL::text IS NULL OR tenant_id = NULL) \
             AND ('{}'::timestamptz IS NULL OR recorded_at <= '{}'::timestamptz) \
             AND (NULL::timestamptz IS NULL OR valid_from <= NULL) \
             ORDER BY seq DESC LIMIT 1",
            oracle_1200[0].to_rfc3339(),
            oracle_1200[0].to_rfc3339(),
        ),
    );
}
