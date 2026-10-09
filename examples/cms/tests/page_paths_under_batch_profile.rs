//! Ledger profiling harness for the settled-path guard that runs inside every
//! page edit (`examples/cms/src/content.rs::page_paths_under`, reached from
//! `update_post_with_revision` -> `guard_page_path`).
//!
//! Renaming or re-parenting a nested page re-checks the canonical path of the
//! edited page **and of every descendant** (a rename rewrites the URL of
//! everything under it). `page_paths_under` computes those paths one page at a
//! time: `page_path_of` issues one `SELECT ... WHERE id = $1` for the page and
//! another for each ancestor on the way to the root. For a subtree of `D`
//! descendants at depth `d` that is `~(D + 1) * (d + 1)` single-row
//! statements, on top of the level-by-level `descendant_ids` walk — all of it
//! inside the transaction that holds the page-hierarchy advisory lock and the
//! edited row's `FOR UPDATE` lock.
//!
//! The harness drives the public entry point `update_post_with_revision` (the
//! function the admin save handler calls) for a slug rename of a page that
//! has 10 / 50 / 250 descendants, and reads `pg_stat_statements` for the
//! whole transaction.
//!
//! **Requires Postgres** with `pg_stat_statements` preloaded: either set
//! `AUTUMN_TEST_PG_URL` to a superuser URL of such a server (a scratch
//! database is dropped and recreated on it), or have Docker for a
//! testcontainer. Run manually with:
//!
//! ```text
//! AUTUMN_TEST_PG_URL=postgres://postgres@127.0.0.1:5544/postgres \
//!   cargo test -p cms --test page_paths_under_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! A ~10,500-row `posts` table (10,000 `post` rows, 70/20/10% publish / draft
//! / trash with `published_at` NULL on every unpublished row, plus 500
//! top-level `page` rows), with real dead tuples from a follow-up `UPDATE` and
//! `ANALYZE` but no `VACUUM`. On top: three disjoint documentation sections,
//! each `root -> edited page -> {children -> grandchildren}` with 10 / 50 /
//! 250 descendants, grandchildren skewed onto few children (`u^2`), plus one
//! trashed descendant and one non-page descendant per tier. A fourth set of
//! edge-case shapes (cycle, non-page ancestor, over-deep chain, top-level
//! page, non-page row, missing id) exists only for the equivalence check.

#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use cms::content::{self, EditContext};
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Nullable, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const MIGRATION_SQL: &str =
    include_str!("../migrations/20260908005714_create_content_schema/up.sql");

/// Split the migration into individual statements (comments stripped first —
/// a prose comment containing a semicolon would otherwise be a boundary).
fn migration_statements() -> Vec<String> {
    let without_comments: String = MIGRATION_SQL
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

fn apply_migration(conn: &mut PgConnection) {
    use diesel::RunQueryDsl;
    for statement in migration_statements() {
        diesel::sql_query(statement)
            .execute(conn)
            .expect("apply cms migration");
    }
}

#[derive(QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

fn insert_row(
    conn: &mut PgConnection,
    post_type: &str,
    status: &str,
    slug: &str,
    parent_id: Option<i64>,
) -> i64 {
    use diesel::RunQueryDsl;
    diesel::sql_query(
        "INSERT INTO posts \
         (post_type, title, slug, excerpt, body, status, author_id, parent_id, \
          published_at, created_at, updated_at) \
         VALUES ($1, $2, $3, '', 'Fixture body.', $4, 1, $5, \
                 TIMESTAMP '2024-01-01 00:00:00', TIMESTAMP '2024-01-01 00:00:00', \
                 TIMESTAMP '2024-01-01 00:00:00') \
         RETURNING id",
    )
    .bind::<Text, _>(post_type)
    .bind::<Text, _>(format!("Title {slug}"))
    .bind::<Text, _>(slug)
    .bind::<Text, _>(status)
    .bind::<Nullable<BigInt>, _>(parent_id)
    .get_result::<IdRow>(conn)
    .expect("insert fixture row")
    .id
}

fn insert_page(conn: &mut PgConnection, slug: &str, parent_id: Option<i64>) -> i64 {
    insert_row(conn, "page", "publish", slug, parent_id)
}

const NUM_BACKGROUND_POSTS: i64 = 10_000;
const NUM_BACKGROUND_PAGES: i64 = 500;

fn seed_background(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO posts \
         (post_type, title, slug, excerpt, body, status, author_id, \
          published_at, created_at, updated_at) \
         SELECT 'post', 'Post ' || i, 'post-' || i, '', \
           'Lorem ipsum dolor sit amet, post number ' || i || '.', \
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
    conn.batch_execute(&format!(
        "INSERT INTO posts \
         (post_type, title, slug, excerpt, body, status, author_id, \
          published_at, created_at, updated_at) \
         SELECT 'page', 'Background Page ' || i, 'bgpage-' || i, '', \
           'Background filler page content ' || i || '.', 'publish', 1, \
           TIMESTAMP '2024-01-01 00:00:00', TIMESTAMP '2024-01-01 00:00:00', \
           TIMESTAMP '2024-01-01 00:00:00' \
         FROM generate_series(1, {NUM_BACKGROUND_PAGES}) AS i"
    ))
    .expect("seed background pages");
    // Real dead tuples: a follow-up UPDATE, then ANALYZE with no VACUUM.
    conn.batch_execute(
        "UPDATE posts SET excerpt = 'reconciled' WHERE id % 7 = 0 AND post_type = 'post'",
    )
    .expect("create dead tuples");
    conn.batch_execute("ANALYZE posts").expect("analyze");
}

struct Tier {
    label: &'static str,
    descendants: usize,
}

struct SeededTier {
    edited_id: i64,
    descendant_ids: Vec<i64>,
}

/// `root -> edited -> children -> grandchildren`, skewed (`u^2`) so a few
/// children own most grandchildren, plus one trashed page and one non-page row
/// under the edited page (neither is filtered by `descendant_ids`).
fn seed_tier(conn: &mut PgConnection, tier: &Tier) -> SeededTier {
    let root = insert_page(conn, &format!("{}-root", tier.label), None);
    let edited = insert_page(conn, &format!("{}-edited", tier.label), Some(root));
    let n_children = (tier.descendants / 5).max(2);
    let n_grand = tier.descendants - n_children;
    let mut descendant_ids = Vec::new();
    let mut children = Vec::new();
    for c in 0..n_children {
        let id = insert_page(conn, &format!("{}-child-{c}", tier.label), Some(edited));
        children.push(id);
        descendant_ids.push(id);
    }
    for g in 0..n_grand {
        let u = (g as f64 + 0.5) / n_grand as f64;
        let parent = children[((n_children as f64) * u * u) as usize];
        descendant_ids.push(insert_page(
            conn,
            &format!("{}-grand-{g}", tier.label),
            Some(parent),
        ));
    }
    descendant_ids.push(insert_row(
        conn,
        "page",
        "trash",
        &format!("{}-trashed", tier.label),
        Some(edited),
    ));
    descendant_ids.push(insert_row(
        conn,
        "post",
        "publish",
        &format!("{}-nonpage", tier.label),
        Some(edited),
    ));
    SeededTier {
        edited_id: edited,
        descendant_ids,
    }
}

/// Shapes `page_paths_under` must treat identically before and after.
fn seed_edge_cases(conn: &mut PgConnection) -> Vec<i64> {
    let mut ids = Vec::new();
    // Top-level page with a child: own path None, child path [top, kid].
    let top = insert_page(conn, "edge-top", None);
    ids.push(top);
    ids.push(insert_page(conn, "edge-top-kid", Some(top)));
    // A non-page ancestor (only reachable by a direct write).
    let plain = insert_row(conn, "post", "publish", "edge-plain-post", None);
    ids.push(plain);
    ids.push(insert_page(conn, "edge-under-post", Some(plain)));
    // A chain deeper than MAX_PAGE_DEPTH (11 levels).
    let mut parent = None;
    for level in 0..11 {
        let id = insert_page(conn, &format!("edge-deep-{level}"), parent);
        ids.push(id);
        parent = Some(id);
    }
    // A pre-existing two-page cycle (only reachable by a direct write).
    let x = insert_page(conn, "edge-cycle-x", None);
    let y = insert_page(conn, "edge-cycle-y", Some(x));
    conn.batch_execute(&format!("UPDATE posts SET parent_id = {y} WHERE id = {x}"))
        .expect("close cycle");
    ids.push(x);
    ids.push(y);
    // A duplicate slug under two different parents.
    let d1 = insert_page(conn, "edge-dup-a", None);
    let d2 = insert_page(conn, "edge-dup-b", None);
    ids.push(insert_page(conn, "same-slug", Some(d1)));
    ids.push(insert_page(conn, "same-slug", Some(d2)));
    ids.push(d1);
    ids.push(d2);
    // A row id that does not exist.
    ids.push(999_999_999);
    ids
}

/// The implementation as it stood before the Ledger change: one `post_by_id`
/// per ancestor, per descendant. Kept verbatim as the equivalence oracle.
async fn legacy_page_paths_under(conn: &mut AsyncPgConnection, post_id: i64) -> Vec<Vec<String>> {
    let mut ids = vec![post_id];
    ids.extend(
        content::descendant_ids(conn, post_id)
            .await
            .expect("descendant_ids"),
    );
    let mut paths = Vec::new();
    for id in ids {
        let Some(post) = content::post_by_id(conn, id).await.expect("post_by_id") else {
            continue;
        };
        if post.post_type != "page" || post.parent_id.is_none() {
            continue;
        }
        let mut segments = vec![post.slug.clone()];
        let mut cursor = post.parent_id;
        let mut seen = vec![post.id];
        while let Some(parent_id) = cursor {
            if segments.len() > content::MAX_PAGE_DEPTH || seen.contains(&parent_id) {
                break;
            }
            seen.push(parent_id);
            let Some(parent) = content::post_by_id(conn, parent_id)
                .await
                .expect("post_by_id")
            else {
                break;
            };
            segments.push(parent.slug.clone());
            cursor = parent.parent_id;
        }
        segments.reverse();
        paths.push(segments);
    }
    paths
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
    hit: i64,
    #[diesel(sql_type = BigInt)]
    read: i64,
    #[diesel(sql_type = BigInt)]
    temp_written: i64,
    #[diesel(sql_type = BigInt)]
    wal_bytes: i64,
    #[diesel(sql_type = BigInt)]
    rows_returned: i64,
}

fn reset_stats(conn: &mut PgConnection) {
    use diesel::RunQueryDsl;
    diesel::sql_query("SELECT pg_stat_statements_reset()")
        .execute(conn)
        .expect("reset pg_stat_statements");
}

fn load_stats(conn: &mut PgConnection) -> Vec<StatementRow> {
    use diesel::RunQueryDsl;
    diesel::sql_query(
        "SELECT query, calls, \
                (shared_blks_hit + shared_blks_read) AS buffers, \
                shared_blks_hit AS hit, shared_blks_read AS read, \
                temp_blks_written AS temp_written, wal_bytes::bigint AS wal_bytes, \
                rows AS rows_returned \
         FROM pg_stat_statements \
         WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC, buffers DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements")
}

fn normalized(q: &str) -> String {
    q.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The statements `page_paths_under` issues: the per-page single-row
/// `SELECT ... FROM posts WHERE id = $1` (no `FOR UPDATE`, which is the row
/// lock at the top of the edit), its batched `id = ANY($1)` replacement, and
/// the `parent_id = ANY($1)` descendant walk.
fn is_target(q: &str) -> bool {
    let q = normalized(q);
    q.starts_with("SELECT")
        && q.contains("FROM \"posts\"")
        && !q.contains("FOR UPDATE")
        && (q.contains("\"posts\".\"id\" = $1")
            || q.contains("\"posts\".\"id\" = ANY")
            || q.contains("\"parent_id\" = ANY"))
}

/// Prints the ranking by calls and by buffers, and the share the target
/// statements hold of the whole transaction. Returns
/// `(total_calls, total_buffers, target_calls, target_buffers)`.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64) {
    let rows = load_stats(conn);
    println!("\n=== pg_stat_statements: {label} ===");
    let total_calls: i64 = rows.iter().map(|r| r.calls).sum();
    let total_buffers: i64 = rows.iter().map(|r| r.buffers).sum();
    let (mut tc, mut tb) = (0i64, 0i64);
    for r in &rows {
        if is_target(&r.query) {
            tc += r.calls;
            tb += r.buffers;
        }
    }
    println!("-- top 10 by calls --");
    for r in rows.iter().take(10) {
        println!(
            "calls={:<6} buffers={:<7} (hit={} read={}) temp_w={} wal={} rows={} target={} :: {}",
            r.calls,
            r.buffers,
            r.hit,
            r.read,
            r.temp_written,
            r.wal_bytes,
            r.rows_returned,
            is_target(&r.query),
            normalized(&r.query)
        );
    }
    let mut by_buffers: Vec<&StatementRow> = rows.iter().collect();
    by_buffers.sort_by_key(|r| std::cmp::Reverse(r.buffers));
    println!("-- top 10 by buffers --");
    for r in by_buffers.iter().take(10) {
        println!(
            "buffers={:<7} calls={:<6} target={} :: {}",
            r.buffers,
            r.calls,
            is_target(&r.query),
            normalized(&r.query)
        );
    }
    let temp: i64 = rows.iter().map(|r| r.temp_written).sum();
    let wal: i64 = rows.iter().map(|r| r.wal_bytes).sum();
    println!(
        "-- TOTAL calls={total_calls} buffers={total_buffers} temp_blks_written={temp} \
         wal_bytes={wal} | TARGET calls={tc} ({:.1}%) buffers={tb} ({:.1}%) --",
        100.0 * tc as f64 / total_calls.max(1) as f64,
        100.0 * tb as f64 / total_buffers.max(1) as f64,
    );
    (total_calls, total_buffers, tc, tb)
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

/// Returns `(url, keepalive)`; the keepalive owns a container when Docker is
/// used.
async fn database_url() -> (String, Option<Box<dyn std::any::Any>>) {
    if let Ok(admin_url) = std::env::var("AUTUMN_TEST_PG_URL") {
        let mut admin = PgConnection::establish(&admin_url).expect("admin connection");
        admin
            .batch_execute("DROP DATABASE IF EXISTS ledger_page_paths")
            .expect("drop scratch db");
        admin
            .batch_execute("CREATE DATABASE ledger_page_paths")
            .expect("create scratch db");
        let (base, _) = admin_url.rsplit_once('/').expect("db in url");
        return (format!("{base}/ledger_page_paths"), None);
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
async fn page_paths_under_batch_profile() {
    let (url, _keepalive) = database_url().await;

    let mut conn = PgConnection::establish(&url).expect("sync db connection");
    conn.batch_execute("CREATE EXTENSION IF NOT EXISTS pg_stat_statements")
        .expect("create pg_stat_statements extension");
    apply_migration(&mut conn);
    conn.batch_execute(
        "INSERT INTO users (username, email, password_hash, display_name, role) \
         VALUES ('author', 'author@example.com', 'x', 'Author', 'administrator')",
    )
    .expect("seed author");
    seed_background(&mut conn);

    let tiers = [
        Tier {
            label: "alpha",
            descendants: 10,
        },
        Tier {
            label: "bravo",
            descendants: 50,
        },
        Tier {
            label: "charlie",
            descendants: 250,
        },
    ];
    let seeded: Vec<SeededTier> = tiers.iter().map(|t| seed_tier(&mut conn, t)).collect();
    let edge_ids = seed_edge_cases(&mut conn);
    conn.batch_execute("ANALYZE posts").expect("analyze");

    let mut aconn = <AsyncPgConnection as diesel_async::AsyncConnection>::establish(&url)
        .await
        .expect("async db connection");

    // ---- Equivalence: new vs legacy, sorted deterministically ------------
    let probe_ids: Vec<i64> = {
        use diesel::RunQueryDsl;
        let mut ids: Vec<i64> = diesel::sql_query(
            "SELECT id FROM posts WHERE post_type = 'page' OR parent_id IS NOT NULL ORDER BY id",
        )
        .load::<IdRow>(&mut conn)
        .expect("probe ids")
        .into_iter()
        .map(|r| r.id)
        .collect();
        ids.extend(edge_ids.iter().copied());
        ids.sort_unstable();
        ids.dedup();
        ids
    };
    let mut compared = 0usize;
    for id in &probe_ids {
        let mut new = content::page_paths_under(&mut aconn, *id)
            .await
            .expect("page_paths_under");
        let mut old = legacy_page_paths_under(&mut aconn, *id).await;
        // Two rows may share a path (duplicate slugs), so sort the whole
        // path list; ties are identical Vecs and cannot be reordered apart.
        new.sort();
        old.sort();
        assert_eq!(new, old, "page_paths_under({id}) diverged from legacy");
        compared += 1;
    }
    println!("\n=== equivalence: {compared} probe ids, new == legacy for every one ===");

    // ---- Workload: rename a nested page that has D descendants -----------
    let mut results = Vec::new();
    for (tier, s) in tiers.iter().zip(&seeded) {
        reset_stats(&mut conn);
        let new_slug = format!("{}-renamed", tier.label);
        let updated = content::update_post_with_revision(
            &mut aconn,
            s.edited_id,
            EditContext {
                editor_id: 1,
                summary: "rename".to_owned(),
                expected_lock_version: None,
                record_revision: true,
                may_touch_hierarchy: false,
                actor: None,
            },
            {
                let new_slug = new_slug.clone();
                move |post| post.slug = new_slug
            },
        )
        .await
        .expect("rename");
        assert_eq!(updated.slug, new_slug);
        let (total_calls, total_buffers, tc, tb) = print_profile(
            &mut conn,
            &format!(
                "rename a nested page with {} descendants (+1 trashed, +1 non-page)",
                tier.descendants
            ),
        );
        results.push((tier.descendants, total_calls, total_buffers, tc, tb));
    }

    println!("\n=== scaling across tiers (whole edit transaction) ===");
    println!(
        "{:>12} {:>12} {:>14} {:>14} {:>16}",
        "descendants", "total calls", "total buffers", "target calls", "target buffers"
    );
    for (d, total_calls, total_buffers, tc, tb) in &results {
        println!("{d:>12} {total_calls:>12} {total_buffers:>14} {tc:>14} {tb:>16}");
    }

    let first_child = seeded[2].descendant_ids[0];
    explain(
        &mut conn,
        "single-row page lookup (page_path_of, one per page and per ancestor)",
        &format!("SELECT * FROM posts WHERE id = {first_child}"),
    );
    explain(
        &mut conn,
        "descendant_ids level query",
        &format!(
            "SELECT id FROM posts WHERE parent_id = ANY(ARRAY[{}])",
            seeded[2].edited_id
        ),
    );
    explain(
        &mut conn,
        "batched row lookup shape",
        &format!(
            "SELECT * FROM posts WHERE id = ANY(ARRAY[{}])",
            seeded[2]
                .descendant_ids
                .iter()
                .take(50)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    );
}
