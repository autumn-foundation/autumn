//! Ledger profiling harness for the per-result permalink resolution in
//! `GET /api/v1/posts` (`examples/cms/src/routes/api.rs::list_posts`).
//!
//! `Repos::permalink` (`examples/cms/src/routes/site.rs`) walks a `page`-typed
//! post's ancestor chain one row at a time via `page_ancestry`, and
//! `list_posts` called it once per result, unbatched. For every `page`-typed
//! hit in a results page, that is up to `MAX_PAGE_DEPTH` (8) sequential
//! single-row `find_by_id` round trips — paid on an unauthenticated,
//! unbounded-traffic public JSON API route (`page` is a registered
//! `public: true` post type, so `?post_type=page` reaches it).
//!
//! The exact same shape was already found and fixed for the nav menu
//! (`site::nav_for`/`content::posts_with_ancestors`) and for the front-end
//! `search()`/`listing()` helpers (`permalink_search_ancestry_batch_profile.rs`)
//! — `list_posts` was the one call site that defect-hunt missed. This harness
//! is the same fixture and measurement shape as that sibling, retargeted at
//! the JSON API route.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p cms --test api_permalink_ancestry_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! A ~10,500-row `posts` table: 10,000 flat `post`-type rows (a realistic
//! status mix — 70% publish, 20% draft, 10% trash, with `published_at` NULL
//! on every unpublished row) and 500 unrelated top-level `page`-type rows, as
//! "the rest of the site" background. Real dead tuples from a follow-up
//! `UPDATE` before `ANALYZE`, no `VACUUM`.
//!
//! On top of that background, three disjoint "documentation section" shapes,
//! one per tier: a shared 8-level ancestor chain (`MAX_PAGE_DEPTH`) with 2, 5
//! and 10 sibling leaf pages hanging off its deepest page. Each tier's leaves
//! carry a unique token in their title found nowhere else in the fixture, so
//! `GET /api/v1/posts?post_type=page&search=<token>` returns exactly that
//! tier's leaves — the same isolation trick
//! `permalink_search_ancestry_batch_profile.rs` uses for `/search`, reused
//! here because `list_posts` dispatches to the identical
//! `content::search_published` when `search` is set.

#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

use autumn_web::config::AutumnConfig;
use autumn_web::test::{TestApp, TestClient};
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Nullable, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use serde::Deserialize;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const MIGRATION_SQL: &str =
    include_str!("../migrations/20260908005714_create_content_schema/up.sql");

/// Split the migration into individual statements — Postgres refuses more
/// than one command per prepared statement. Comments are stripped first: a
/// prose comment containing a semicolon would otherwise become a statement
/// boundary. Mirrors `tests/integration_test.rs`'s `migration_statements`.
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

/// Insert one `page`-type row, bound rather than interpolated even though
/// every value here is a fixture literal, matching the framework's own
/// parameter-binding convention (`content::search_published`).
fn insert_page(conn: &mut PgConnection, title: &str, slug: &str, parent_id: Option<i64>) -> i64 {
    use diesel::RunQueryDsl;
    diesel::sql_query(
        "INSERT INTO posts \
         (post_type, title, slug, excerpt, body, status, author_id, parent_id, \
          published_at, created_at, updated_at) \
         VALUES ('page', $1, $2, '', 'Fixture page body.', 'publish', 1, $3, \
                 TIMESTAMP '2024-01-01 00:00:00', TIMESTAMP '2024-01-01 00:00:00', \
                 TIMESTAMP '2024-01-01 00:00:00') \
         RETURNING id",
    )
    .bind::<Text, _>(title)
    .bind::<Text, _>(slug)
    .bind::<Nullable<BigInt>, _>(parent_id)
    .get_result::<IdRow>(conn)
    .expect("insert fixture page")
    .id
}

/// One tier: `leaves` sibling pages sharing one `MAX_PAGE_DEPTH`-deep ancestor
/// chain, each carrying `token` somewhere findable only in this tier.
struct Tier {
    token: &'static str,
    chain_label: &'static str,
    leaves: usize,
}

const MAX_PAGE_DEPTH: usize = 8;

/// Build one tier's chain-plus-leaves shape and return the leaf slugs, in
/// creation order, for the equivalence check.
fn seed_tier(conn: &mut PgConnection, tier: &Tier) -> Vec<String> {
    let mut parent: Option<i64> = None;
    for level in 0..MAX_PAGE_DEPTH {
        let id = insert_page(
            conn,
            &format!("{} section level {level}", tier.chain_label),
            &format!("{}-chain-{level}", tier.token.to_lowercase()),
            parent,
        );
        parent = Some(id);
    }
    let deepest = parent.expect("chain has at least one level");
    (0..tier.leaves)
        .map(|n| {
            let slug = format!("{}-leaf-{n}", tier.token.to_lowercase());
            insert_page(
                conn,
                &format!("{} guide {n}", tier.token),
                &slug,
                Some(deepest),
            );
            slug
        })
        .collect()
}

const NUM_BACKGROUND_POSTS: i64 = 10_000;
const NUM_BACKGROUND_PAGES: i64 = 500;

/// The "rest of the site": flat posts with a realistic status mix and NULL
/// density on `published_at`, plus unrelated top-level pages.
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
         SELECT \
           'page', \
           'Background Page ' || i, \
           'bgpage-' || i, \
           '', \
           'Background filler page content ' || i || '.', \
           'publish', \
           1, \
           TIMESTAMP '2024-01-01 00:00:00', \
           TIMESTAMP '2024-01-01 00:00:00', \
           TIMESTAMP '2024-01-01 00:00:00' \
         FROM generate_series(1, {NUM_BACKGROUND_PAGES}) AS i"
    ))
    .expect("seed background pages");

    // Real dead tuples: touch a slice of rows post-insert, same technique the
    // export-CSV and offline-sync Ledger harnesses use.
    conn.batch_execute(
        "UPDATE posts SET excerpt = 'reconciled' WHERE id % 7 = 0 AND post_type = 'post'",
    )
    .expect("create dead tuples");
    conn.batch_execute("ANALYZE posts").expect("analyze");
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

/// Prints every `posts`-touching statement from this run and splits it into
/// the three shapes that matter — see
/// `permalink_search_ancestry_batch_profile.rs::print_profile` for the full
/// rationale, reused verbatim here:
///
/// - `search()`'s own two statements (`websearch_to_tsquery`) — fixed at 2
///   calls regardless of which implementation is under test.
/// - `search_published`'s own row-fetch (`"posts"."id" = ANY($1)`) — always
///   exactly 1 call per request under both implementations; also the shape
///   `posts_with_ancestors`'s batched fix emits, so `any_calls` is reported
///   gross with that 1-call baseline documented.
/// - the single-row `"posts"."id" = $1` shape — 100% attributable to
///   `Repos::permalink`/`page_ancestry`'s unbatched ancestor walk: 0 under the
///   batched fix, `leaves * MAX_PAGE_DEPTH` under the unbatched baseline.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%posts%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut single_calls, mut single_buffers) = (0i64, 0i64);
    let (mut any_calls, mut any_buffers) = (0i64, 0i64);
    let (mut search_calls, mut search_buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        if normalized.contains("websearch_to_tsquery") {
            search_calls += row.calls;
            search_buffers += row.buffers;
        } else if normalized.contains("\"id\" = ANY($1)") {
            any_calls += row.calls;
            any_buffers += row.buffers;
        } else if normalized.contains("\"id\" = $1") {
            single_calls += row.calls;
            single_buffers += row.buffers;
        }
    }
    println!(
        "-- search statements: calls={search_calls} buffers={search_buffers} -- \
         single-row \"id = $1\" (unbatched ancestor walk): calls={single_calls} \
         buffers={single_buffers} -- batched \"id = ANY($1)\" (search's own \
         row-fetch, always 1 call, plus any batched ancestor calls): \
         calls={any_calls} buffers={any_buffers} --"
    );
    (single_calls, single_buffers, any_calls)
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

#[derive(Deserialize, Debug)]
struct ApiPostView {
    slug: String,
    url: String,
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn api_permalink_ancestry_batch_profile() {
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
    apply_migration(&mut conn);

    conn.batch_execute(
        "INSERT INTO users (username, email, password_hash, display_name, role) \
         VALUES ('author', 'author@example.com', 'x', 'Author', 'administrator')",
    )
    .expect("seed author");

    seed_background(&mut conn);

    let tiers = [
        Tier {
            token: "Zqualvora",
            chain_label: "Alphadocs",
            leaves: 2,
        },
        Tier {
            token: "Zqbrenthil",
            chain_label: "Bravodocs",
            leaves: 5,
        },
        Tier {
            token: "Zqcortanix",
            chain_label: "Charliedocs",
            leaves: 10,
        },
    ];
    let mut tier_leaf_slugs = Vec::new();
    for tier in &tiers {
        tier_leaf_slugs.push(seed_tier(&mut conn, tier));
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

    let mut tier_results = Vec::new();
    for (tier, leaf_slugs) in tiers.iter().zip(&tier_leaf_slugs) {
        reset_stats(&mut conn);
        let body: Vec<ApiPostView> = client
            .get(&format!(
                "/api/v1/posts?post_type=page&search={}&per_page=20",
                tier.token
            ))
            .send()
            .await
            .assert_ok()
            .json();
        assert_eq!(
            body.len(),
            leaf_slugs.len(),
            "tier {} must return exactly its own leaves",
            tier.token
        );
        let expected_path = format!(
            "/{}-chain-0/{}-chain-1/{}-chain-2/{}-chain-3/{}-chain-4/\
             {}-chain-5/{}-chain-6/{}-chain-7/",
            tier.token.to_lowercase(),
            tier.token.to_lowercase(),
            tier.token.to_lowercase(),
            tier.token.to_lowercase(),
            tier.token.to_lowercase(),
            tier.token.to_lowercase(),
            tier.token.to_lowercase(),
            tier.token.to_lowercase(),
        );
        for leaf_slug in leaf_slugs {
            let hit = body
                .iter()
                .find(|p| &p.slug == leaf_slug)
                .unwrap_or_else(|| panic!("tier {} missing leaf {leaf_slug}", tier.token));
            let expected_url = format!("{expected_path}{leaf_slug}");
            assert_eq!(
                hit.url, expected_url,
                "tier {} leaf {leaf_slug} must render its full 8-level path",
                tier.token
            );
        }
        let (single_calls, single_buffers, any_calls) = print_profile(
            &mut conn,
            &format!(
                "tier {} ({} leaves, shared 8-level chain)",
                tier.token, tier.leaves
            ),
        );
        tier_results.push((
            tier.token,
            tier.leaves,
            single_calls,
            single_buffers,
            any_calls,
        ));
    }

    println!("\n=== statement-count scaling across tiers ===");
    println!(
        "{:<12} {:>8} {:>14} {:>16} {:>28}",
        "tier",
        "leaves",
        "\"id = $1\"",
        "\"id = $1\" buffers",
        "\"id = ANY($1)\" (incl. search's own 1)"
    );
    for (token, leaves, single_calls, single_buffers, any_calls) in &tier_results {
        println!("{token:<12} {leaves:>8} {single_calls:>14} {single_buffers:>16} {any_calls:>28}");
    }

    explain(
        &mut conn,
        "single-row ancestor lookup shape (find_by_id, one per level per leaf, unbatched)",
        "SELECT * FROM posts WHERE id = 1",
    );
    explain(
        &mut conn,
        "batched ancestor lookup shape (posts_with_ancestors, one per level for the whole batch)",
        "SELECT * FROM posts WHERE id = ANY(ARRAY[1, 2, 3])",
    );
}
