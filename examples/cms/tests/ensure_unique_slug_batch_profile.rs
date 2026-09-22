//! Ledger profiling harness for `content::ensure_unique_slug`
//! (`examples/cms/src/content.rs`), called directly (the real function, not a
//! hand-rolled duplicate query) against a real Postgres fixture.
//!
//! Before the fix in this same PR, `ensure_unique_slug` walks candidate
//! suffixes (`desired`, `desired-2`, `desired-3`, …) one at a time in a `for`
//! loop, issuing a separate `SELECT COUNT(*)` round trip per candidate until
//! it finds one that is free — up to 199 sequential statements for a single
//! slug allocation. It is called from every admin post/page create and
//! slug-changing edit (`routes/admin/posts.rs`) and from every public-facing
//! content creation (`routes/site.rs`, in a retry loop that can call it up to
//! 5 times on a lost race) — a real, hot, public write path. A title with
//! many prior collisions (a recurring "Weekly Update" title, a date-stamped
//! post, a bulk/scripted import) is not an exotic case; it is the common one
//! this defect is expensive on.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p cms --test ensure_unique_slug_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! 300 unrelated background `post`-type rows (`bg-post-1` … `bg-post-300`),
//! large enough that `idx_posts_bare_path_slug` / `idx_posts_type_slug` have a
//! realistic size instead of trivially fitting on one page, plus 60
//! pre-existing collisions on the profiled slug itself: `some-title`,
//! `some-title-2`, … `some-title-60` — the shape a recurring title or a
//! scripted import naturally produces. Real dead tuples from a follow-up
//! `UPDATE` before `ANALYZE`, no `VACUUM`, matching every other Ledger
//! fixture in this repo.
//!
//! The profiled call is `ensure_unique_slug(conn, "post", "some-title", None,
//! None)`, which — with 60 existing collisions occupying `some-title` through
//! `some-title-60` — must walk 61 candidates (`some-title` through
//! `some-title-61`) before finding `some-title-61` free.
//!
//! A second block (not profiled for statement counts, run after the main
//! profile) proves result-equivalence across every edge case the fix must not
//! change: no collision, one collision, the exact 198/199 boundary (the
//! pre-existing off-by-one where `desired-200` is never itself queried), the
//! `shadowed_by_a_route` case, `exclude_id`, and the nested-page-vs-bare-path
//! scoping rules.

#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

use cms::content::ensure_unique_slug;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Nullable, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const MIGRATION_SQL: &str =
    include_str!("../migrations/20260908005714_create_content_schema/up.sql");

/// Mirrors `permalink_search_ancestry_batch_profile.rs`'s helper of the same
/// name (and `tests/integration_test.rs`'s `migration_statements`).
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

/// Insert one post row, bound rather than interpolated even though most
/// values here are fixture literals, matching `permalink_search_ancestry_batch_profile.rs`'s
/// `insert_page`.
fn insert_post(
    conn: &mut PgConnection,
    post_type: &str,
    title: &str,
    slug: &str,
    parent_id: Option<i64>,
) -> i64 {
    use diesel::RunQueryDsl;
    diesel::sql_query(
        "INSERT INTO posts \
         (post_type, title, slug, excerpt, body, status, author_id, parent_id, \
          published_at, created_at, updated_at) \
         VALUES ($1, $2, $3, '', 'Fixture body.', 'publish', 1, $4, \
                 TIMESTAMP '2024-01-01 00:00:00', TIMESTAMP '2024-01-01 00:00:00', \
                 TIMESTAMP '2024-01-01 00:00:00') \
         RETURNING id",
    )
    .bind::<Text, _>(post_type)
    .bind::<Text, _>(title)
    .bind::<Text, _>(slug)
    .bind::<Nullable<BigInt>, _>(parent_id)
    .get_result::<IdRow>(conn)
    .expect("insert fixture post")
    .id
}

const NUM_BACKGROUND_POSTS: i64 = 300;
const NUM_COLLISIONS: u32 = 60;

/// 300 unrelated background posts, plus 60 pre-existing collisions on
/// `some-title` (`some-title`, `some-title-2`, … `some-title-60`) — the shape
/// a recurring title or a scripted import naturally produces. Real dead
/// tuples from a follow-up `UPDATE` before `ANALYZE`, no `VACUUM`.
fn seed_background(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO posts \
         (post_type, title, slug, excerpt, body, status, author_id, \
          published_at, created_at, updated_at) \
         SELECT \
           'post', 'Background Post ' || i, 'bg-post-' || i, '', \
           'Background filler content ' || i || '.', 'publish', 1, \
           TIMESTAMP '2024-01-01 00:00:00', TIMESTAMP '2024-01-01 00:00:00', \
           TIMESTAMP '2024-01-01 00:00:00' \
         FROM generate_series(1, {NUM_BACKGROUND_POSTS}) AS i"
    ))
    .expect("seed background posts");

    insert_post(conn, "post", "Some Title", "some-title", None);
    for suffix in 2..=NUM_COLLISIONS {
        insert_post(
            conn,
            "post",
            &format!("Some Title {suffix}"),
            &format!("some-title-{suffix}"),
            None,
        );
    }

    // Real dead tuples: touch a slice of rows post-insert, same technique
    // every other Ledger fixture in this repo uses, no intervening VACUUM.
    conn.batch_execute("UPDATE posts SET excerpt = 'reconciled' WHERE id % 7 = 0")
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
/// the two shapes that matter:
///
/// - the single-candidate `"slug" = $1` COUNT shape (no `ANY(`) — the pre-fix
///   per-suffix probe, one call per candidate tried. 0 under the fix.
/// - the batched `"slug" = ANY($` shape — the fix's one query for the whole
///   candidate list.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64) {
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
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        if normalized.contains("\"slug\" = ANY(") {
            any_calls += row.calls;
            any_buffers += row.buffers;
        } else if normalized.contains("\"slug\" = $1") {
            single_calls += row.calls;
            single_buffers += row.buffers;
        }
    }
    println!(
        "-- single-candidate slug probe (unbatched, one call per suffix tried): \
         calls={single_calls} buffers={single_buffers} -- batched slug ANY() probe \
         (the fix, one call for the whole candidate list): calls={any_calls} \
         buffers={any_buffers} --"
    );
    (single_calls, single_buffers, any_calls, any_buffers)
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
async fn ensure_unique_slug_batch_profile() {
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

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(config).build().expect("pool");

    cms::bootstrap();

    // === Profiled call: 60 pre-existing collisions, so the (pre-fix) loop
    // must walk 61 candidates (`some-title` through `some-title-61`) before
    // it finds `some-title-61` free. ===
    reset_stats(&mut conn);
    let mut async_conn = pool.get().await.expect("async conn");
    let allocated = ensure_unique_slug(&mut async_conn, "post", "some-title", None, None)
        .await
        .expect("a free slug must be found within the 199-candidate budget");
    assert_eq!(
        allocated, "some-title-61",
        "with 60 pre-existing collisions (some-title..some-title-60), the first \
         free candidate in enumeration order must be some-title-61"
    );
    drop(async_conn);
    let profile = print_profile(&mut conn, "60 pre-existing collisions -> some-title-61");

    println!("\n=== statement-count summary ===");
    println!(
        "{:<45} {:>12} {:>14} {:>10} {:>12}",
        "scenario", "single calls", "single buffers", "any calls", "any buffers"
    );
    println!(
        "{:<45} {:>12} {:>14} {:>10} {:>12}",
        "ensure_unique_slug (60 collisions)", profile.0, profile.1, profile.2, profile.3
    );

    explain(
        &mut conn,
        "single-candidate slug probe shape (pre-fix, one call per suffix tried)",
        "SELECT count(*) FROM posts WHERE slug = 'some-title-30' AND post_type = ANY(ARRAY['post', 'page']) \
         AND (post_type = 'post' OR parent_id IS NULL)",
    );
    let any_list = std::iter::once("'some-title'".to_owned())
        .chain((2..=61u32).map(|s| format!("'some-title-{s}'")))
        .collect::<Vec<_>>()
        .join(",");
    explain(
        &mut conn,
        "batched slug ANY() probe shape (the fix, one call for the whole candidate list)",
        &format!(
            "SELECT slug FROM posts WHERE slug = ANY(ARRAY[{any_list}]) \
             AND post_type = ANY(ARRAY['post', 'page']) AND (post_type = 'post' OR parent_id IS NULL)"
        ),
    );

    // === Equivalence checks (not part of the profiled statement count above) ===
    // Every scenario uses slugs disjoint from the profiled fixture above, so
    // none of them can interact with it.
    let mut async_conn = pool.get().await.expect("async conn");

    // No collision: the desired slug is returned immediately.
    let solo = ensure_unique_slug(&mut async_conn, "post", "solo-title", None, None)
        .await
        .expect("no collision");
    assert_eq!(solo, "solo-title");

    // One collision: `desired` taken, `desired-2` free.
    insert_post(&mut conn, "post", "Duet", "duet-title", None);
    let duet = ensure_unique_slug(&mut async_conn, "post", "duet-title", None, None)
        .await
        .expect("one collision");
    assert_eq!(duet, "duet-title-2");

    // The exact 198/199 boundary. 198 taken (`boundary` through
    // `boundary-198`) must still succeed with `boundary-199` — the last
    // candidate the original loop would ever reach.
    insert_post(&mut conn, "post", "Boundary", "boundary", None);
    for suffix in 2..=198u32 {
        insert_post(
            &mut conn,
            "post",
            &format!("Boundary {suffix}"),
            &format!("boundary-{suffix}"),
            None,
        );
    }
    let at_198 = ensure_unique_slug(&mut async_conn, "post", "boundary", None, None)
        .await
        .expect("198 taken must still leave boundary-199 free");
    assert_eq!(
        at_198, "boundary-199",
        "198 pre-existing collisions must resolve to the 199th candidate"
    );
    // Now take the 199th too (`boundary-199` itself): every candidate the
    // loop would ever check is taken, so the call must fail with the
    // existing error, not silently reach for `boundary-200`.
    insert_post(&mut conn, "post", "Boundary 199", "boundary-199", None);
    let exhausted = ensure_unique_slug(&mut async_conn, "post", "boundary", None, None).await;
    let error = exhausted.expect_err("199 taken candidates must exhaust the budget");
    assert_eq!(
        error.to_string(),
        "Too many posts share this slug; choose a different one",
        "the boundary error message must be unchanged"
    );

    // `shadowed_by_a_route`: `search` is a literal reserved application
    // route (`is_reserved_path`), so a bare-path type desiring it starts its
    // search at `-2`, not at the bare word.
    let shadowed = ensure_unique_slug(&mut async_conn, "post", "search", None, None)
        .await
        .expect("shadowed slug still resolves");
    assert_eq!(
        shadowed, "search-2",
        "a route-shadowed desired slug must start its candidate search at -2"
    );

    // `exclude_id`: editing a post must not collide with itself.
    let edit_id = insert_post(&mut conn, "post", "Edit Me", "edit-me", None);
    let self_edit = ensure_unique_slug(&mut async_conn, "post", "edit-me", None, Some(edit_id))
        .await
        .expect("a post excluded by id does not collide with its own row");
    assert_eq!(self_edit, "edit-me");

    // Nested-page scoping: a page named "team" under one parent does not
    // collide with a page named "team" under a *different* parent.
    let parent_a = insert_post(&mut conn, "page", "Parent A", "parent-a", None);
    let parent_b = insert_post(&mut conn, "page", "Parent B", "parent-b", None);
    insert_post(&mut conn, "page", "Team", "team", Some(parent_a));
    let team_under_b = ensure_unique_slug(&mut async_conn, "page", "team", Some(parent_b), None)
        .await
        .expect("a nested page competes only with its own siblings");
    assert_eq!(
        team_under_b, "team",
        "a page under a different parent must not collide with a same-named sibling elsewhere"
    );

    // Bare-path scoping: a top-level `post` and a top-level `page` DO
    // compete for the same bare URL, across `BARE_PATH_TYPES`.
    insert_post(&mut conn, "post", "Showcase", "showcase", None);
    let showcase_page = ensure_unique_slug(&mut async_conn, "page", "showcase", None, None)
        .await
        .expect("bare-path types compete across post and page");
    assert_eq!(
        showcase_page, "showcase-2",
        "a top-level page must be renamed when a post already holds the bare path"
    );

    println!("\nAll equivalence checks passed.");
}
