//! Ledger profiling harness for the admin revision-history screen
//! (`GET /admin/content/{type}/{id}/revisions`, `routes::admin::posts::revisions`).
//!
//! Hypothesis: after loading the revisions in one statement the handler credits
//! each *distinct* `revisions.author_id` with its own `users.find_by_id`, so a
//! post edited by `k` accounts costs `k` single-row statements for what is one
//! `users.id = ANY(...)` lookup. The count is per request and grows with how
//! many editors have touched the post.
//!
//! **Requires Postgres** with `pg_stat_statements` preloaded: set
//! `AUTUMN_TEST_PG_URL` to a superuser URL of such a server (a scratch database
//! is dropped and recreated on it), or have Docker for a testcontainer:
//!
//! ```text
//! AUTUMN_TEST_PG_URL=postgres://postgres@127.0.0.1:5433/postgres \
//!   cargo test -p cms --test revisions_author_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! 50,000 `posts` (70/20/10% publish/draft/trash, NULL `published_at` on the
//! unpublished, dead tuples, no `VACUUM`), 2,000 `users`, and ~150,000
//! background `revisions` over 20,000 posts with ~5% NULL `author_id` and a
//! squared skew onto few editors. Three measured posts carry 60 revisions each
//! by exactly 1, 4 and 12 distinct editors, plus NULL-author revisions and an
//! editor with an empty `display_name`, so the statement count is shown scaling
//! with `k` before the change and flat after it.

#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::too_many_lines
)]

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
const INDEX_MIGRATION: &str =
    include_str!("../migrations/20260925180000_add_posts_author_status_published_index/up.sql");

const NUM_BACKGROUND_POSTS: i64 = 50_000;
const NUM_USERS: i64 = 2_000;
const REVISIONS_PER_MEASURED_POST: i64 = 60;
/// Distinct-editor counts of the three measured posts.
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

/// Seeds the production-shaped background and the three measured posts.
/// Returns `(post_id, distinct_editors, editor_names)` per measured post.
fn seed(conn: &mut PgConnection) -> Vec<(i64, i64, Vec<String>)> {
    use diesel::RunQueryDsl;

    // User 1 (the administrator) already exists: the harness registers it
    // through the route so the session is real.
    conn.batch_execute(&format!(
        "INSERT INTO users (username, email, password_hash, display_name, role) \
         SELECT 'editor' || u, 'editor' || u || '@example.com', 'x', \
                CASE WHEN u % 50 = 0 THEN '' ELSE 'Editor Name ' || u END, 'editor' \
         FROM generate_series(1, {NUM_USERS}) AS u"
    ))
    .expect("seed users");

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

    // ~150k background revisions over 20,000 posts (7-8 each), ~5% NULL author,
    // squared skew onto few editors (floor(u^2 * N) + 2: ids 2..).
    conn.batch_execute(&format!(
        "SELECT setseed(0.42); \
         INSERT INTO revisions (post_id, title, excerpt, body, status, author_id, summary, created_at) \
         SELECT (g % 20000) + 1, 'Rev ' || g, '', 'Revision body ' || g, 'draft', \
                CASE WHEN g % 20 = 0 THEN NULL \
                     ELSE floor(random() * random() * {NUM_USERS})::bigint + 2 END, \
                'Edited', \
                TIMESTAMP '2024-06-01 00:00:00' + (g || ' seconds')::interval \
         FROM generate_series(1, 150000) AS g"
    ))
    .expect("seed background revisions");

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
        // Editors are users 50*j + 2 .. so some tiers include display_name = ''
        // accounts (user id = 51 -> username editor50, empty display name).
        let editor_ids: Vec<i64> = (0..k).map(|j| 49 + j * 3 + k).collect();
        let ids_sql = editor_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        conn.batch_execute(&format!(
            "INSERT INTO revisions (post_id, title, excerpt, body, status, author_id, summary, created_at) \
             SELECT {id}, 'Measured {k} rev ' || r, '', 'body ' || r, 'publish', \
                    CASE WHEN r % 15 = 0 THEN NULL \
                         ELSE (ARRAY[{ids_sql}])[1 + floor(r::float8 * r / {n})::int % {k}] END, \
                    'Edited', TIMESTAMP '2025-06-01' + (r || ' minutes')::interval \
             FROM generate_series(1, {n}) AS r",
            n = REVISIONS_PER_MEASURED_POST
        ))
        .expect("seed measured revisions");
        measured.push((id, k, editor_ids));
    }
    conn.batch_execute("ANALYZE").expect("analyze");

    #[derive(QueryableByName)]
    struct NameRow {
        #[diesel(sql_type = Text)]
        username: String,
        #[diesel(sql_type = Text)]
        display_name: String,
    }
    measured
        .into_iter()
        .map(|(id, k, ids)| {
            let names = ids
                .iter()
                .map(|uid| {
                    let row = diesel::sql_query(format!(
                        "SELECT username, display_name FROM users WHERE id = {uid}"
                    ))
                    .get_result::<NameRow>(conn)
                    .expect("editor row");
                    if row.display_name.is_empty() {
                        row.username
                    } else {
                        row.display_name
                    }
                })
                .collect();
            (id, k, names)
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
    #[diesel(sql_type = BigInt)]
    wal: i64,
    #[diesel(sql_type = BigInt)]
    temp_written: i64,
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

/// Prints the request's statement profile ranked by calls and returns
/// `(calls, buffers, author_calls, author_buffers)`; the `author_*` pair is the
/// editor-name resolution slice (per-editor `find_by_id` or its batched form).
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} (ranked by calls) ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers, \
                wal_bytes::bigint AS wal, temp_blks_written AS temp_written \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC, buffers DESC, query",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut calls, mut buffers, mut a_calls, mut a_buffers, mut wal, mut temp) =
        (0, 0, 0, 0, 0, 0);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        let is_author = normalized.contains("FROM \"users\"")
            && (normalized.contains("\"users\".\"id\" = $1")
                || normalized.contains("\"users\".\"id\" = ANY")
                || normalized.contains("\"users\".\"id\" IN"));
        println!(
            "calls={:<4} buffers={:<5} wal={:<6}{} {normalized}",
            row.calls,
            row.buffers,
            row.wal,
            if is_author { " [AUTHOR-RESOLVE]" } else { "" }
        );
        calls += row.calls;
        buffers += row.buffers;
        wal += row.wal;
        temp += row.temp_written;
        if is_author {
            a_calls += row.calls;
            a_buffers += row.buffers;
        }
    }
    println!(
        "-- request total: statements={calls} buffers={buffers} wal_bytes={wal} \
         temp_blks_written={temp}; author-resolve slice: statements={a_calls} \
         ({:.1}% of calls) buffers={a_buffers} ({:.1}% of buffers) --",
        100.0 * a_calls as f64 / calls.max(1) as f64,
        100.0 * a_buffers as f64 / buffers.max(1) as f64,
    );
    (calls, buffers, a_calls, a_buffers)
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

/// `(url, keepalive)`; the keepalive owns the container when Docker is used.
async fn database_url() -> (String, Option<Box<dyn std::any::Any>>) {
    if let Ok(admin_url) = std::env::var("AUTUMN_TEST_PG_URL") {
        let mut admin = PgConnection::establish(&admin_url).expect("admin connection");
        admin
            .batch_execute("DROP DATABASE IF EXISTS ledger_revisions_author")
            .expect("drop scratch db");
        admin
            .batch_execute("CREATE DATABASE ledger_revisions_author")
            .expect("create scratch db");
        let (base, _) = admin_url.rsplit_once('/').expect("db in url");
        return (format!("{base}/ledger_revisions_author"), None);
    }
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
    (
        format!("postgres://postgres:postgres@{host}:{port}/postgres"),
        Some(Box::new(container)),
    )
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn revisions_author_batch_profile() {
    let (url, _keepalive) = database_url().await;

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .expect("create pg_stat_statements extension");
    apply_migration(&mut conn, BASE_SCHEMA);
    apply_migration(&mut conn, INDEX_MIGRATION);

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

    // The first registered account owns the site (user id 1, administrator).
    let resp = client
        .post("/register")
        .form("username=owner&email=owner%40example.com&password=correct-horse-battery-staple")
        .send()
        .await;
    assert_eq!(resp.status, 303, "registration: {}", resp.text());

    let measured = seed(&mut conn);

    // Warm lazy statics / the settings memo so measured requests are steady state.
    client
        .get(&format!("/admin/content/post/{}/revisions", measured[0].0))
        .send()
        .await
        .assert_ok();

    explain(
        &mut conn,
        "revisions for the 12-editor post",
        &format!(
            "SELECT * FROM revisions WHERE post_id = {} ORDER BY created_at DESC, id DESC",
            measured[2].0
        ),
    );
    explain(
        &mut conn,
        "one users.id = $1 lookup (per-editor find_by_id shape)",
        "SELECT * FROM users WHERE users.id = 52",
    );
    explain(
        &mut conn,
        "one users.id = ANY($1) lookup (12 ids)",
        "SELECT * FROM users WHERE users.id = ANY(ARRAY[61,64,67,70,73,76,79,82,85,88,91,94])",
    );

    println!(
        "\n#################### MEASURED: GET /admin/content/post/{{id}}/revisions ####################"
    );
    let mut summary = Vec::new();
    for (id, k, names) in &measured {
        reset_stats(&mut conn);
        let body = client
            .get(&format!("/admin/content/post/{id}/revisions"))
            .send()
            .await
            .assert_ok()
            .text();
        // Result equivalence: every editor is credited by name in the history.
        for name in names {
            assert!(
                body.contains(name.as_str()),
                "history for {k} editors must credit `{name}`"
            );
        }
        let (calls, buffers, a_calls, a_buffers) =
            print_profile(&mut conn, &format!("k={k} editors (post id {id})"));
        println!(
            "-- body: {} bytes, hash {:016x} --",
            body.len(),
            body_hash(&body)
        );
        summary.push((*k, calls, buffers, a_calls, a_buffers));
    }

    println!("\n=== summary (per request) ===");
    println!(
        "{:<4} {:>12} {:>14} {:>18} {:>20}",
        "k", "stmts total", "buffers total", "stmts author-res", "buffers author-res"
    );
    for (k, calls, buffers, a_calls, a_buffers) in &summary {
        println!("{k:<4} {calls:>12} {buffers:>14} {a_calls:>18} {a_buffers:>20}");
    }
}
