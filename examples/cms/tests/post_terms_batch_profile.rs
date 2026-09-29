//! Ledger profiling harness for the public single-post view
//! (`examples/cms/src/routes/front.rs`'s `single_post`, reached here through
//! `GET /archives/{id}`), specifically `Repos::post_terms`
//! (`examples/cms/src/routes/site.rs`), which resolves the terms a post is
//! filed under.
//!
//! Hypothesis: `post_terms` reads the post's `post_terms` links in one
//! statement and then resolves each link's term with its own `find_by_id`,
//! so a post filed under `k` terms costs `1 + k` statements for what is one
//! `terms.id = ANY(...)` lookup. The count is per request, on an
//! unauthenticated route every reader hits, and grows with how heavily an
//! editor tags the post.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p cms --test post_terms_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! The ~50,500-row `posts` background this crate's other Ledger harnesses
//! use (70% publish / 20% draft / 10% trash, NULL `published_at` on the
//! non-published, real dead tuples, no `VACUUM`), plus 500 `terms` and a
//! skewed `post_terms` fan-out: every published post carries `id % 6` terms
//! (0-5, so ~1/6 carry none). Three measured posts are added on top with
//! exactly 1, 4 and 12 terms so the statement count is shown scaling with
//! `k` in the baseline and staying flat afterwards.

#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

use std::hash::{Hash, Hasher};

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

const NUM_BACKGROUND_POSTS: i64 = 50_000;
const NUM_TERMS: i64 = 500;
/// Term counts of the three measured posts.
const TIERS: [i64; 3] = [1, 4, 12];

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

#[derive(QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

fn seed(conn: &mut PgConnection) -> Vec<(i64, i64)> {
    use diesel::RunQueryDsl;

    conn.batch_execute(
        "INSERT INTO users (username, email, password_hash, display_name, role) \
         VALUES ('author', 'author@example.com', 'x', 'Author', 'administrator')",
    )
    .expect("seed author");

    conn.batch_execute(&format!(
        "INSERT INTO posts \
         (post_type, title, slug, excerpt, body, status, author_id, \
          published_at, created_at, updated_at) \
         SELECT 'post', 'Post ' || i, 'post-' || i, '', \
           'Lorem ipsum dolor sit amet, consectetur adipiscing elit, post number ' || i || '.', \
           CASE WHEN i % 10 < 7 THEN 'publish' WHEN i % 10 < 9 THEN 'draft' ELSE 'trash' END, \
           1, \
           CASE WHEN i % 10 < 7 \
                THEN TIMESTAMP '2024-01-01 00:00:00' + (i || ' minutes')::interval \
                ELSE NULL END, \
           TIMESTAMP '2024-01-01 00:00:00' + (i || ' minutes')::interval, \
           TIMESTAMP '2024-01-01 00:00:00' + (i || ' minutes')::interval \
         FROM generate_series(1, {NUM_BACKGROUND_POSTS}) AS i"
    ))
    .expect("seed background posts");
    conn.batch_execute("UPDATE posts SET excerpt = 'reconciled' WHERE id % 7 = 0")
        .expect("create dead tuples");

    conn.batch_execute(&format!(
        "INSERT INTO terms (taxonomy, name, slug, description) \
         SELECT CASE WHEN t % 4 = 0 THEN 'category' ELSE 'post_tag' END, \
                'Term ' || t, 'term-' || t, '' \
         FROM generate_series(1, {NUM_TERMS}) AS t"
    ))
    .expect("seed terms");

    // Skewed fan-out: `id % 6` distinct terms per published post. Stride 13
    // is coprime with 500, so a post's terms never collide with each other.
    conn.batch_execute(&format!(
        "INSERT INTO post_terms (post_id, term_id) \
         SELECT p.id, ((p.id * 7 + j * 13) % {NUM_TERMS}) + 1 \
         FROM posts p, generate_series(0, 4) AS j \
         WHERE p.status = 'publish' AND j < p.id % 6"
    ))
    .expect("seed post_terms");

    let mut measured = Vec::new();
    for k in TIERS {
        let id = diesel::sql_query(format!(
            "INSERT INTO posts (post_type, title, slug, excerpt, body, status, author_id, \
                                published_at, created_at, updated_at) \
             VALUES ('post', 'Measured {k}', 'measured-{k}', '', 'measured body', 'publish', 1, \
                     TIMESTAMP '2025-06-01', TIMESTAMP '2025-06-01', TIMESTAMP '2025-06-01') \
             RETURNING id"
        ))
        .get_result::<IdRow>(conn)
        .expect("insert measured post")
        .id;
        conn.batch_execute(&format!(
            "INSERT INTO post_terms (post_id, term_id) \
             SELECT {id}, j * 37 + 5 FROM generate_series(0, {}) AS j",
            k - 1
        ))
        .expect("attach measured terms");
        measured.push((id, k));
    }

    conn.batch_execute("ANALYZE").expect("analyze");
    measured
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

#[derive(QueryableByName)]
struct ExplainLine {
    #[diesel(sql_type = Text, column_name = "QUERY PLAN")]
    line: String,
}

fn reset_stats(conn: &mut PgConnection) {
    use diesel::RunQueryDsl;
    diesel::sql_query("SELECT pg_stat_statements_reset()")
        .execute(conn)
        .expect("reset pg_stat_statements");
}

/// Prints the whole request's statement profile ranked by calls, and returns
/// `(total_calls, total_buffers, terms_calls, terms_buffers)` where the
/// `terms_*` pair is the `post_terms`-resolution slice: the per-term
/// `find_by_id` lookups (or their batched replacement).
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} (ranked by calls) ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC, buffers DESC, query",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut calls, mut buffers, mut t_calls, mut t_buffers) = (0, 0, 0, 0);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        let is_terms = normalized.contains("FROM \"terms\"")
            && (normalized.contains("\"terms\".\"id\" = $1")
                || normalized.contains("\"terms\".\"id\" = ANY")
                || normalized.contains("\"terms\".\"id\" IN"));
        println!(
            "calls={:<4} buffers={:<5}{} {normalized}",
            row.calls,
            row.buffers,
            if is_terms { " [TERM-RESOLVE]" } else { "" }
        );
        calls += row.calls;
        buffers += row.buffers;
        if is_terms {
            t_calls += row.calls;
            t_buffers += row.buffers;
        }
    }
    println!(
        "-- request total: statements={calls} buffers={buffers}; term-resolve slice: \
         statements={t_calls} ({:.1}% of calls) buffers={t_buffers} ({:.1}% of buffers) --",
        100.0 * t_calls as f64 / calls.max(1) as f64,
        100.0 * t_buffers as f64 / buffers.max(1) as f64,
    );
    (calls, buffers, t_calls, t_buffers)
}

fn explain(conn: &mut PgConnection, label: &str, sql: &str) {
    use diesel::RunQueryDsl;
    println!("\n=== EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS): {label} ===");
    let lines = diesel::sql_query(format!(
        "EXPLAIN (ANALYZE, BUFFERS, VERBOSE, SETTINGS) {sql}"
    ))
    .load::<ExplainLine>(conn)
    .expect("explain");
    for line in lines {
        println!("{}", line.line);
    }
}

fn body_hash(body: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn post_terms_batch_profile() {
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
    let measured = seed(&mut conn);

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

    // Warm every lazy static / prepared-statement cache the route touches so
    // the measured requests see steady-state statement counts.
    client.get("/archives/1").send().await;

    // The two statement shapes at issue, planned against the real fixture:
    // the per-term lookup `find_by_id` issues, and the single `ANY` lookup
    // that resolves every term at once.
    explain(
        &mut conn,
        "post_terms links for the 12-term post",
        &format!("SELECT * FROM post_terms WHERE post_id = {}", measured[2].0),
    );
    explain(
        &mut conn,
        "one terms.id = $1 lookup (per-term find_by_id shape)",
        "SELECT * FROM terms WHERE terms.id = 5",
    );
    explain(
        &mut conn,
        "one terms.id = ANY($1) lookup (12 ids)",
        "SELECT * FROM terms WHERE terms.id = ANY(ARRAY[5,42,79,116,153,190,227,264,301,338,375,412])",
    );

    println!("\n#################### MEASURED: GET /archives/{{id}} ####################");
    let mut summary = Vec::new();
    for (id, k) in &measured {
        reset_stats(&mut conn);
        let body = client
            .get(&format!("/archives/{id}"))
            .send()
            .await
            .assert_ok()
            .text();
        assert!(
            body.contains(&format!("Measured {k}")),
            "single-post view must render the measured post (k={k}):\n{}",
            &body[..body.len().min(2000)]
        );
        // Result equivalence: every attached term must be rendered.
        let expected: Vec<String> = (0..*k).map(|j| format!("Term {}", j * 37 + 5)).collect();
        for name in &expected {
            assert!(
                body.contains(name.as_str()),
                "post with {k} terms must render `{name}`"
            );
        }
        let (calls, buffers, t_calls, t_buffers) =
            print_profile(&mut conn, &format!("k={k} terms (post id {id})"));
        println!(
            "-- body: {} bytes, hash {:016x} --",
            body.len(),
            body_hash(&body)
        );
        summary.push((*k, calls, buffers, t_calls, t_buffers));
    }

    println!("\n=== summary (per request) ===");
    println!(
        "{:<4} {:>16} {:>14} {:>18} {:>18}",
        "k", "stmts total", "buffers total", "stmts term-resolve", "buffers term-resolve"
    );
    for (k, calls, buffers, t_calls, t_buffers) in &summary {
        println!("{k:<4} {calls:>16} {buffers:>14} {t_calls:>18} {t_buffers:>18}");
    }
}
