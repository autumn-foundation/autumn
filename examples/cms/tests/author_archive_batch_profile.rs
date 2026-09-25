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
//! That is cheap when the requested author accounts for a large share of the
//! site's published posts (most rows visited match) and increasingly
//! expensive the smaller that author's share is — which is the *common*
//! case on a real multi-author site: a handful of staff writers account for
//! most of the archive, and most bylines are occasional contributors with a
//! handful of posts each. This harness measures that gap directly across
//! three author-share tiers on one fixture, then (in the fix commit) adds a
//! covering `(author_id, status, published_at DESC)` index and re-measures
//! in the same session.
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

const BASE_SCHEMA: &str =
    include_str!("../migrations/20260908005714_create_content_schema/up.sql");

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
        TIERS[0].modulus,
        user_ids[0],
        TIERS[1].modulus,
        user_ids[1],
        TIERS[2].modulus,
        user_ids[2],
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
    println!("-- total (author_id-filtered posts statements): calls={total_calls} buffers={total_buffers} --");
    (total_calls, total_buffers)
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
    println!("\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): {label} (author_id={author_id}) ===");
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
        explain_row_fetch(
            &mut conn,
            &format!("baseline / {username} ({published} published)"),
            *user_id,
        );
        baseline_shares.push((*username, calls, buffers));
    }

    println!("\n=== baseline summary (calls, buffers per tier) ===");
    for (username, calls, buffers) in &baseline_shares {
        println!("{username:<12} calls={calls:<6} buffers={buffers}");
    }
}
