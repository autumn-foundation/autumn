//! Ledger profiling harness for `/author/{username}`
//! (`examples/cms/src/routes/front.rs`'s `author_archive`, backed by
//! `content::published_post_count_by_author`/`published_posts_by_author` in
//! `examples/cms/src/content.rs`), driven through the real route.
//!
//! Both functions filter `posts` by `(author_id, status, post_type)`; the row
//! fetch additionally orders by `published_at DESC, id DESC` with a
//! `LIMIT`/`OFFSET` for pagination. Two indexes exist on `posts` today:
//! `idx_posts_author` (`author_id` alone — no status, no order) and
//! `idx_posts_status_published` (`status`, `published_at DESC` — the sort
//! order, but no author filter). Neither covers the combination this query
//! needs, so the planner's only real choice for the row fetch is to walk
//! `idx_posts_status_published` in `published_at` order, checking every
//! row's `author_id` and discarding the ones that don't match, until it has
//! collected a page's worth.
//!
//! For the *row fetch*, that is cheap when the requested author accounts for
//! a large share of the site's published posts (most rows visited match) and
//! increasingly expensive the smaller that author's share is. For the
//! *count* (no `LIMIT` to stop early), it is the other way around: it has to
//! visit every matching row regardless of share, so it costs the most,
//! measured, for the *most* prolific author — 7,426 of that tier's 7,440
//! total buffers were the count, not the row fetch. Both are real, and both
//! are common: a handful of staff writers account for most of a real
//! multi-author site's archive, and most bylines are occasional
//! contributors with a handful of posts each — so both the "prolific
//! author's page renders" case and the "most authors' pages render" case
//! are hot in practice. This harness measures both across three
//! author-share tiers on one fixture, then (in the fix commit) adds a
//! covering `(author_id, status, published_at DESC) INCLUDE (post_type)`
//! index — the `INCLUDE` lets the count run as an Index Only Scan once
//! autovacuum sets the visibility map, with no heap fetch at all — and
//! re-measures in the same session.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p cms --test author_archive_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! The same ~50,500-row `posts` background this crate's other Ledger
//! harnesses use (`permalink_search_ancestry_batch_profile.rs`,
//! `ensure_unique_slug_batch_profile.rs`): 50,000 `post`-type rows with a
//! realistic status mix (70% publish, 20% draft, 10% trash — draft/trash
//! rows carry no `published_at`) plus real dead tuples from a follow-up
//! `UPDATE` before `ANALYZE`, no `VACUUM`.
//!
//! On top of that, three authors' published posts are carved out of the
//! background by `author_id % N` selectors chosen to land close to:
//!
//! | tier       | username      | published posts | share of ~35k published |
//! |------------|---------------|-----------------:|-------------------------:|
//! | prolific   | `staffwriter` |          ~11,600 |                     ~33% |
//! | mid-tier   | `contributor` |            ~660 |                    ~1.9% |
//! | guest      | `guestauthor` |             ~15 |                   ~0.04% |
//!
//! The selectors are mutually exclusive (evaluated rarest-first in one
//! `CASE`), so the three tiers never share a row.

#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

use autumn_web::config::AutumnConfig;
use autumn_web::test::{TestApp, TestClient};
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const BASE_SCHEMA: &str = include_str!("../migrations/20260908005714_create_content_schema/up.sql");
const AUTHOR_INDEX_MIGRATION: &str =
    include_str!("../migrations/20260925180000_add_posts_author_status_published_index/up.sql");

/// Split a migration file into individual statements — Postgres refuses more
/// than one command per prepared statement, and (for `CREATE INDEX
/// CONCURRENTLY`, added in the fix commit) each statement must run as its
/// own implicit-autocommit unit rather than batched into one multi-statement
/// round trip. Comments are stripped first: a prose comment containing a
/// semicolon would otherwise become a statement boundary. Mirrors
/// `tests/integration_test.rs`'s `migration_statements`.
fn migration_statements(sql: &str) -> Vec<String> {
    let without_comments: String = sql
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    without_comments
        .split(';')
        .map(|statement| statement.trim().to_owned())
        .filter(|statement| !statement.is_empty())
        .collect()
}

fn apply_migration(conn: &mut PgConnection, sql: &str) {
    use diesel::RunQueryDsl;
    for statement in migration_statements(sql) {
        diesel::sql_query(statement)
            .execute(conn)
            .unwrap_or_else(|e| panic!("apply migration statement failed: {e}\n{sql}"));
    }
}

const NUM_BACKGROUND_POSTS: i64 = 50_000;

/// The default author every background post starts under, before the
/// per-tier `UPDATE` below carves the three measured authors out of it.
const DEFAULT_AUTHOR_ID: i64 = 1;

/// Flat posts with a realistic status mix and NULL density on
/// `published_at`, plus real dead tuples — the same background every other
/// Ledger CMS harness seeds.
fn seed_background(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO posts \
         (post_type, title, slug, excerpt, body, status, author_id, \
          published_at, created_at, updated_at) \
         SELECT \
           'post', \
           'Post ' || i, \
           'post-' || i, \
           '', \
           'Lorem ipsum dolor sit amet, consectetur adipiscing elit, post number ' || i || '.', \
           CASE WHEN i % 10 < 7 THEN 'publish' \
                WHEN i % 10 < 9 THEN 'draft' \
                ELSE 'trash' END, \
           {DEFAULT_AUTHOR_ID}, \
           CASE WHEN i % 10 < 7 \
                THEN TIMESTAMP '2024-01-01 00:00:00' + (i || ' minutes')::interval \
                ELSE NULL END, \
           TIMESTAMP '2024-01-01 00:00:00' + (i || ' minutes')::interval, \
           TIMESTAMP '2024-01-01 00:00:00' + (i || ' minutes')::interval \
         FROM generate_series(1, {NUM_BACKGROUND_POSTS}) AS i"
    ))
    .expect("seed background posts");

    // Real dead tuples: touch a slice of rows post-insert, same technique the
    // other Ledger harnesses use.
    conn.batch_execute("UPDATE posts SET excerpt = 'reconciled' WHERE id % 7 = 0")
        .expect("create dead tuples");
}

/// One measured author: its handle, and the modulus that selects which
/// background rows become its published posts (see the module doc's tier
/// table). Evaluated rarest-modulus-first in one `CASE` so the tiers can
/// never overlap.
struct Tier {
    username: &'static str,
    modulus: i64,
}

const TIERS: [Tier; 3] = [
    Tier {
        username: "guestauthor",
        modulus: 2377,
    },
    Tier {
        username: "contributor",
        modulus: 53,
    },
    Tier {
        username: "staffwriter",
        modulus: 3,
    },
];

#[derive(QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

/// Create the three measured authors and carve their published posts out of
/// the background, rarest tier first so the moduli never collide. Returns
/// each tier's `(username, user_id, published_count)`.
fn seed_authors(conn: &mut PgConnection) -> Vec<(&'static str, i64, i64)> {
    use diesel::RunQueryDsl;

    let mut user_ids = Vec::new();
    for tier in &TIERS {
        let id = diesel::sql_query(
            "INSERT INTO users (username, email, password_hash, display_name, role) \
             VALUES ($1, $1 || '@example.com', 'x', $1, 'author') RETURNING id",
        )
        .bind::<Text, _>(tier.username)
        .get_result::<IdRow>(conn)
        .expect("insert tier author")
        .id;
        user_ids.push(id);
    }

    // Rarest-first CASE: a row matching `guestauthor`'s modulus never falls
    // through to `contributor`'s or `staffwriter`'s, even though 2377 is a
    // multiple of neither 53 nor 3 in a way that would collide here, because
    // once a WHEN matches no later WHEN is evaluated for that row.
    conn.batch_execute(&format!(
        "UPDATE posts SET author_id = CASE \
           WHEN id % {} = 0 THEN {} \
           WHEN id % {} = 0 THEN {} \
           WHEN id % {} = 0 THEN {} \
           ELSE author_id END \
         WHERE status = 'publish'",
        TIERS[0].modulus, user_ids[0], TIERS[1].modulus, user_ids[1], TIERS[2].modulus, user_ids[2],
    ))
    .expect("carve out tier authors");

    conn.batch_execute("ANALYZE posts").expect("analyze");

    #[derive(QueryableByName)]
    struct CountRow {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }
    TIERS
        .iter()
        .zip(&user_ids)
        .map(|(tier, &user_id)| {
            let n = diesel::sql_query(
                "SELECT count(*) AS n FROM posts WHERE author_id = $1 AND status = 'publish'",
            )
            .bind::<BigInt, _>(user_id)
            .get_result::<CountRow>(conn)
            .expect("count tier's published posts")
            .n;
            (tier.username, user_id, n)
        })
        .collect()
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
    use diesel::RunQueryDsl;
    diesel::sql_query("SELECT pg_stat_statements_reset()")
        .execute(conn)
        .expect("reset pg_stat_statements");
}

/// Prints every `posts`-by-`author_id` statement pg_stat_statements recorded
/// this run — `published_post_count_by_author`'s gate check,
/// `published_posts_by_author`'s own internal count (identical SQL shape to
/// the gate's, so `pg_stat_statements` folds both under one row — `calls` is
/// therefore 2 per request), and `published_posts_by_author`'s row fetch —
/// and returns `(total_calls, total_buffers)` across all of them, the
/// harness's own workload total this run's target share is computed
/// against.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%posts%' AND query ILIKE '%author_id%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let mut total_calls = 0i64;
    let mut total_buffers = 0i64;
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        total_calls += row.calls;
        total_buffers += row.buffers;
    }
    println!(
        "-- total (author_id-filtered posts statements): calls={total_calls} buffers={total_buffers} --"
    );
    (total_calls, total_buffers)
}

/// Sum of every statement `pg_stat_statements` recorded since the last
/// reset, across the *whole* workload (not just the `author_id`-filtered
/// subset `print_profile` isolates) — the denominator the Hard Gate's
/// "% of total buffers / % of total calls" needs. Excludes the meta query
/// itself, same as `print_profile`.
fn total_workload_buffers_calls(conn: &mut PgConnection) -> (i64, i64) {
    use diesel::RunQueryDsl;
    #[derive(QueryableByName)]
    struct TotalsRow {
        #[diesel(sql_type = BigInt)]
        calls: i64,
        #[diesel(sql_type = BigInt)]
        buffers: i64,
    }
    let row = diesel::sql_query(
        "SELECT COALESCE(SUM(calls), 0)::bigint AS calls, \
                COALESCE(SUM(shared_blks_hit + shared_blks_read), 0)::bigint AS buffers \
         FROM pg_stat_statements WHERE query NOT ILIKE '%pg_stat_statements%'",
    )
    .get_result::<TotalsRow>(conn)
    .expect("total workload buffers/calls");
    (row.calls, row.buffers)
}

/// The WAL cost of inserting `n` ordinary posts, measured via the
/// `pg_current_wal_lsn()` delta around the batch — the admissible technique
/// for "any write-path claim" (Ledger process), used here to price the new
/// index's tax on every future insert into `posts`.
fn wal_bytes_for_insert_batch(
    conn: &mut PgConnection,
    label: &str,
    author_id: i64,
    slug_prefix: &str,
    offset: i64,
    n: i64,
) -> i64 {
    use diesel::RunQueryDsl;

    #[derive(QueryableByName)]
    struct LsnRow {
        #[diesel(sql_type = Text)]
        lsn: String,
    }
    let before = diesel::sql_query("SELECT pg_current_wal_lsn()::text AS lsn")
        .get_result::<LsnRow>(conn)
        .expect("wal lsn before")
        .lsn;

    conn.batch_execute(&format!(
        "INSERT INTO posts \
         (post_type, title, slug, excerpt, body, status, author_id, created_at, updated_at) \
         SELECT 'post', 'WAL bench ' || i, '{slug_prefix}-' || i, '', 'x', 'draft', \
                {author_id}, NOW(), NOW() \
         FROM generate_series({start}, {end}) AS i",
        start = offset + 1,
        end = offset + n,
    ))
    .expect("wal bench insert batch");

    #[derive(QueryableByName)]
    struct DiffRow {
        #[diesel(sql_type = BigInt)]
        bytes: i64,
    }
    let bytes = diesel::sql_query(format!(
        "SELECT pg_wal_lsn_diff(pg_current_wal_lsn(), '{before}'::pg_lsn)::bigint AS bytes"
    ))
    .get_result::<DiffRow>(conn)
    .expect("wal lsn diff")
    .bytes;
    println!("=== WAL bytes for {n} inserts ({label}): {bytes} ===");
    bytes
}

/// `idx_scan` for `idx_posts_author_status_published` — confirms the new
/// index is actually chosen by the planner in this session, not merely
/// created and left unused.
///
/// `pg_stat_force_next_flush()` first: PG 15+ keeps per-backend statistics in
/// local memory and only flushes them to the shared stats snapshot
/// `pg_stat_user_indexes` reads from at transaction end or on its own
/// ~500ms timer — so a check performed immediately after the `EXPLAIN
/// EXECUTE` calls above, even on the same connection, can read a snapshot
/// that has not absorbed them yet and report `idx_scan=0` despite the plan
/// visibly using the index. Forcing the flush makes this deterministic
/// instead of a timing-dependent flake.
fn author_status_published_idx_scan(conn: &mut PgConnection) -> i64 {
    use diesel::RunQueryDsl;
    #[derive(QueryableByName)]
    struct ScanRow {
        #[diesel(sql_type = BigInt)]
        idx_scan: i64,
    }
    diesel::sql_query("SELECT pg_stat_force_next_flush()")
        .execute(conn)
        .expect("force stats flush");
    diesel::sql_query(
        "SELECT idx_scan FROM pg_stat_user_indexes \
         WHERE indexrelname = 'idx_posts_author_status_published'",
    )
    .get_result::<ScanRow>(conn)
    .map(|row| row.idx_scan)
    .unwrap_or(0)
}

/// The new index's on-disk size in bytes, for the write-cost report.
fn author_status_published_index_size(conn: &mut PgConnection) -> i64 {
    use diesel::RunQueryDsl;
    #[derive(QueryableByName)]
    struct SizeRow {
        #[diesel(sql_type = BigInt)]
        bytes: i64,
    }
    diesel::sql_query(
        "SELECT pg_relation_size('idx_posts_author_status_published')::bigint AS bytes",
    )
    .get_result::<SizeRow>(conn)
    .expect("index size")
    .bytes
}

#[derive(QueryableByName, Debug)]
struct ExplainLine {
    #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
    line: String,
}

/// `EXPLAIN` the real row-fetch shape with a *bound* parameter (`$1`), the
/// same extended-query-protocol path Diesel's `.bind()` uses in production —
/// unlike a literal-interpolated value, a correlated subquery or an ad hoc
/// `psql` constant, which can make the planner choose a different, unrealistic
/// plan. `PREPARE`/`EXECUTE` is the only way to get that exact protocol shape
/// from a plain SQL string.
fn explain_row_fetch(conn: &mut PgConnection, label: &str, author_id: i64) {
    use diesel::RunQueryDsl;
    println!(
        "\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): {label} (author_id={author_id}) ==="
    );
    diesel::sql_query(
        "PREPARE ledger_author_page (bigint) AS \
         SELECT \"posts\".* FROM \"posts\" \
         WHERE \"posts\".\"author_id\" = $1 \
           AND \"posts\".\"status\" = 'publish' \
           AND \"posts\".\"post_type\" = ANY(ARRAY['post', 'page']) \
         ORDER BY \"posts\".\"published_at\" DESC, \"posts\".\"id\" DESC \
         LIMIT 10 OFFSET 0",
    )
    .execute(conn)
    .expect("prepare");
    let lines = diesel::sql_query(format!(
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) EXECUTE ledger_author_page({author_id})"
    ))
    .load::<ExplainLine>(conn)
    .expect("explain");
    for line in lines {
        println!("{}", line.line);
    }
    diesel::sql_query("DEALLOCATE ledger_author_page")
        .execute(conn)
        .expect("deallocate");
}

/// `EXPLAIN` the count shape (identical predicate, no `ORDER BY`/`LIMIT`) —
/// the query with no `LIMIT` to exploit, so it is the one the row-fetch's
/// early-exit plan does nothing for. See `explain_row_fetch` for why this
/// uses `PREPARE`/`EXECUTE` rather than a literal or subquery.
fn explain_count(conn: &mut PgConnection, label: &str, author_id: i64) {
    use diesel::RunQueryDsl;
    println!(
        "\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): {label} count (author_id={author_id}) ==="
    );
    diesel::sql_query(
        "PREPARE ledger_author_count (bigint) AS \
         SELECT count(*) FROM \"posts\" \
         WHERE \"posts\".\"author_id\" = $1 \
           AND \"posts\".\"status\" = 'publish' \
           AND \"posts\".\"post_type\" = ANY(ARRAY['post', 'page'])",
    )
    .execute(conn)
    .expect("prepare");
    let lines = diesel::sql_query(format!(
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) EXECUTE ledger_author_count({author_id})"
    ))
    .load::<ExplainLine>(conn)
    .expect("explain");
    for line in lines {
        println!("{}", line.line);
    }
    diesel::sql_query("DEALLOCATE ledger_author_count")
        .execute(conn)
        .expect("deallocate");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn author_archive_batch_profile() {
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
    apply_migration(&mut conn, BASE_SCHEMA);

    conn.batch_execute(
        "INSERT INTO users (username, email, password_hash, display_name, role) \
         VALUES ('author', 'author@example.com', 'x', 'Author', 'administrator')",
    )
    .expect("seed default author");

    seed_background(&mut conn);
    let tiers = seed_authors(&mut conn);
    for (username, user_id, published) in &tiers {
        println!("tier {username} (user_id={user_id}): {published} published posts");
    }

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(config).build().expect("pool");

    cms::bootstrap();
    let mut app_config = AutumnConfig::default();
    app_config.security.csrf.enabled = false;
    let client: TestClient = TestApp::new()
        .routes(cms::all_routes())
        .config(app_config)
        .with_db(pool)
        .build();

    println!("\n#################### BASELINE (no author-covering index) ####################");
    let mut baseline_shares = Vec::new();
    let mut baseline_bodies = Vec::new();
    for (username, user_id, published) in &tiers {
        reset_stats(&mut conn);
        let body = client
            .get(&format!("/author/{username}"))
            .send()
            .await
            .assert_ok()
            .text();
        assert!(
            body.contains("Post "),
            "author archive for {username} must render at least one post card:\n{}",
            &body[..body.len().min(2000)]
        );
        let (calls, buffers) = print_profile(&mut conn, &format!("baseline / {username}"));
        let (total_calls, total_buffers) = total_workload_buffers_calls(&mut conn);
        println!(
            "-- share of this request's own workload: calls {calls}/{total_calls} \
             ({:.1}%), buffers {buffers}/{total_buffers} ({:.1}%) --",
            100.0 * calls as f64 / total_calls.max(1) as f64,
            100.0 * buffers as f64 / total_buffers.max(1) as f64,
        );
        explain_row_fetch(
            &mut conn,
            &format!("baseline / {username} ({published} published)"),
            *user_id,
        );
        explain_count(
            &mut conn,
            &format!("baseline / {username} ({published} published)"),
            *user_id,
        );
        baseline_shares.push((*username, calls, buffers));
        baseline_bodies.push((*username, body));
    }

    println!("\n=== baseline summary (calls, buffers per tier) ===");
    for (username, calls, buffers) in &baseline_shares {
        println!("{username:<12} calls={calls:<6} buffers={buffers}");
    }

    let wal_before = wal_bytes_for_insert_batch(
        &mut conn,
        "baseline (no covering index)",
        DEFAULT_AUTHOR_ID,
        "wal-bench-before",
        0,
        2_000,
    );

    println!("\n#################### FIX: apply the covering index ####################");
    apply_migration(&mut conn, AUTHOR_INDEX_MIGRATION);
    // `VACUUM` (not just `ANALYZE`): the count query's Index Only Scan needs
    // the visibility map set to skip heap fetches entirely — the whole point
    // of `INCLUDE (post_type)`. A real table gets this from autovacuum over
    // time; this harness forces it immediately so the "after" numbers show
    // the index's steady-state benefit rather than its cold, not-yet-vacuumed
    // one.
    conn.batch_execute("VACUUM ANALYZE posts")
        .expect("vacuum analyze after adding the covering index");

    let wal_after = wal_bytes_for_insert_batch(
        &mut conn,
        "after (covering index present)",
        DEFAULT_AUTHOR_ID,
        "wal-bench-after",
        2_000,
        2_000,
    );
    println!(
        "\n=== WAL write-cost of the new index: {wal_before} -> {wal_after} bytes for 2,000 \
         inserts ({:+.1}%) ===",
        100.0 * (wal_after - wal_before) as f64 / wal_before.max(1) as f64
    );
    println!(
        "=== new index on-disk size: {} bytes ===",
        author_status_published_index_size(&mut conn)
    );

    println!("\n#################### AFTER (covering index present) ####################");
    let mut after_shares = Vec::new();
    let mut after_bodies = Vec::new();
    for (username, user_id, published) in &tiers {
        reset_stats(&mut conn);
        let body = client
            .get(&format!("/author/{username}"))
            .send()
            .await
            .assert_ok()
            .text();
        assert!(
            body.contains("Post "),
            "author archive for {username} must render at least one post card:\n{}",
            &body[..body.len().min(2000)]
        );
        let (calls, buffers) = print_profile(&mut conn, &format!("after / {username}"));
        let (total_calls, total_buffers) = total_workload_buffers_calls(&mut conn);
        println!(
            "-- share of this request's own workload: calls {calls}/{total_calls} \
             ({:.1}%), buffers {buffers}/{total_buffers} ({:.1}%) --",
            100.0 * calls as f64 / total_calls.max(1) as f64,
            100.0 * buffers as f64 / total_buffers.max(1) as f64,
        );
        explain_row_fetch(
            &mut conn,
            &format!("after / {username} ({published} published)"),
            *user_id,
        );
        explain_count(
            &mut conn,
            &format!("after / {username} ({published} published)"),
            *user_id,
        );
        after_shares.push((*username, calls, buffers));
        after_bodies.push((*username, body));
    }

    println!("\n=== equivalence: baseline vs. after HTML body, per tier ===");
    for ((username, before_body), (username2, after_body)) in
        baseline_bodies.iter().zip(&after_bodies)
    {
        assert_eq!(username, username2, "tier order must match between phases");
        assert_eq!(
            before_body, after_body,
            "the covering index must not change {username}'s rendered author-archive page \
             at all — same rows, same order, same HTML"
        );
        println!("{username:<12} identical ({} bytes)", before_body.len());
    }

    // `pg_stat_user_indexes.idx_scan` is fed by each backend's periodic stats
    // flush (Postgres 15+'s shared-memory stats subsystem reports at most
    // once per `PGSTAT_MIN_INTERVAL`, ~1s, not synchronously per statement),
    // and the archive requests ran on the connection *pool*'s backends, not
    // this harness's own `conn` — so a query issued immediately after the
    // loop above can race the flush and see a stale 0. Poll rather than
    // sleep-and-hope: succeeds the moment the flush lands, and still fails
    // loudly (not silently passes) if the index is genuinely never chosen.
    let mut idx_scan = 0;
    for _ in 0..20 {
        idx_scan = author_status_published_idx_scan(&mut conn);
        if idx_scan > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        idx_scan > 0,
        "idx_posts_author_status_published must actually be chosen by the planner at least \
         once in this run — idx_scan={idx_scan} after polling for 5s means it would be a pure \
         write tax nobody reads (the reddit-clone covering-index negative-result outcome)"
    );
    println!("\n=== idx_posts_author_status_published: idx_scan={idx_scan} (must be > 0) ===");

    println!("\n=== delta summary: baseline -> after (calls, buffers per tier) ===");
    println!(
        "{:<12} {:>16} {:>16} {:>16} {:>16} {:>10}",
        "tier", "calls before", "calls after", "buffers before", "buffers after", "delta %"
    );
    for ((username, bcalls, bbuffers), (_, acalls, abuffers)) in
        baseline_shares.iter().zip(&after_shares)
    {
        let delta_pct = 100.0 * (*abuffers - *bbuffers) as f64 / (*bbuffers).max(1) as f64;
        println!(
            "{username:<12} {bcalls:>16} {acalls:>16} {bbuffers:>16} {abuffers:>16} {delta_pct:>9.1}%"
        );
    }
}
