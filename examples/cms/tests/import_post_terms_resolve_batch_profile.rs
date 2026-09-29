//! Ledger profiling harness for `resolve_import_terms`
//! (`examples/cms/src/routes/admin/tools.rs`), driven through the real
//! `POST /admin/tools/import` route — the same "Import" button handler
//! `import_terms_batch_profile.rs` and `import_revisions_meta_batch_profile.rs`
//! exercise, restoring a site-export JSON file (the `Export`/`ExportPost`
//! payload shape).
//!
//! Before the fix in this same PR, `resolve_import_terms` walks one post's
//! `terms: Vec<ExportTermRef>` and, for **each** reference, calls
//! `repos.terms.find_by_slug(...)` — one single-row `SELECT` per (post, term)
//! pair, filtered to the right taxonomy in application code afterward. It is
//! called once per post in the import's main loop (`tools.rs::import`, both
//! the "resume an unfinished row" branch and the "create a new row" branch),
//! so a file with `P` posts averaging `T` terms each pays `P * T` round
//! trips for term resolution alone — the exact N+1-on-read shape this
//! repo's own `CLAUDE.md` calls out, and the same defect class
//! `import_terms_batch_profile.rs` already fixed one call up (creating the
//! term *rows*, not resolving a post's *references* to them). Everything
//! else in this same import loop already follows the "load once, not per
//! post" rule its own comments state (`imported_source_slugs`,
//! `completed_imports`, `legacy_marker_index`) — this was the one path that
//! didn't.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p cms --test import_post_terms_resolve_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! 4,000 pre-existing background `terms` rows (2,000 `category`, 2,000
//! `post_tag`, slugs prefixed `legacy-`) so the `(taxonomy, slug)` index this
//! call leans on has a realistic size instead of trivially fitting on one
//! page, plus real dead tuples from a follow-up `UPDATE` before `ANALYZE`
//! (no `VACUUM`) — the same technique every other Ledger fixture in this
//! repo uses.
//!
//! The import file itself declares a 60-tag `post_tag` vocabulary (a
//! realistic, reused-across-posts tag set — not one unique tag per post) and
//! 600 draft posts, each referencing 5 of those 60 tags (chosen so every tag
//! is reused roughly 50 times, the "heavily tagged blog" shape
//! `import_terms_batch_profile.rs`'s own doc comment describes). That is
//! 3,000 term references across the file: 3,000 single-row lookups pre-fix,
//! a handful of batched `slug = ANY(...)` queries post-fix. Every post lands
//! as `status: "draft"` (matching `import_revisions_meta_batch_profile.rs`'s
//! convention) so the import performs no status transition, keeping the
//! profiled statement counts attributable to term resolution rather than
//! mixed with `transition_status`'s own writes.
//!
//! The file is imported once, against a site with none of its posts yet —
//! `resolve_import_terms`'s "create a new row" call site. (Its sibling call
//! site, for resuming a previous run's unfinished row, shares the exact same
//! function and the exact same fix; `mark_imports_complete` marks every
//! created post finished at the end of a successful run, so re-uploading the
//! same file afterward takes the "already finished, leave it alone" branch
//! before term resolution runs at all rather than reaching the resume
//! branch — profiling that path needs a *partial* run, which is exercised
//! functionally, not for buffers, elsewhere in this suite.)

#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

use autumn_web::config::AutumnConfig;
use autumn_web::test::{TestApp, TestClient, TestResponse};
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const MIGRATION_SQL: &str =
    include_str!("../migrations/20260908005714_create_content_schema/up.sql");

/// Mirrors `import_terms_batch_profile.rs`'s helper of the same name.
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

/// URL-encode a form field. Mirrors `tests/integration_test.rs`'s `form`.
fn form(pairs: &[(&str, &str)]) -> String {
    fn encode(value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        for byte in value.as_bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(*byte as char);
                }
                b' ' => out.push('+'),
                other => out.push_str(&format!("%{other:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The `name=value` pair from a response's session cookie. Mirrors
/// `tests/integration_test.rs`'s `session_cookie`.
fn session_cookie(resp: &TestResponse) -> String {
    resp.header("set-cookie")
        .expect("an authenticating response sets a session cookie")
        .split(';')
        .next()
        .expect("cookie has a name=value pair")
        .to_owned()
}

/// Register the first account, which the CMS makes site owner
/// (`Role::Administrator`, every capability including `ImportContent`).
async fn register(client: &TestClient, username: &str) -> String {
    let email = format!("{username}@example.com");
    let resp = client
        .post("/register")
        .form(&form(&[
            ("username", username),
            ("email", &email),
            ("password", "correct-horse-battery-staple"),
        ]))
        .send()
        .await;
    assert_eq!(
        resp.status,
        303,
        "registration should redirect; body was: {}",
        resp.text()
    );
    session_cookie(&resp)
}

/// Upload an export file to the importer. Mirrors
/// `import_terms_batch_profile.rs`'s `import_export`.
async fn import_export(client: &TestClient, cookie: &str, payload: &str) -> TestResponse {
    const BOUNDARY: &str = "----cmsimport";
    let body = format!(
        "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"payload\"; \
         filename=\"export.json\"\r\nContent-Type: application/json\r\n\r\n\
         {payload}\r\n--{BOUNDARY}--\r\n"
    );
    client
        .post("/admin/tools/import")
        .header("cookie", cookie)
        .header(
            "content-type",
            &format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .body(body)
        .send()
        .await
}

const NUM_LEGACY_CATEGORIES: i64 = 2_000;
const NUM_LEGACY_TAGS: i64 = 2_000;

/// The site's taxonomy before the restore: 4,000 unrelated pre-existing
/// terms, so the `(taxonomy, slug)` index this call leans on has a realistic
/// size instead of trivially fitting on one page.
fn seed_background(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO terms (taxonomy, name, slug, description, parent_id, created_at) \
         SELECT 'category', 'Legacy Category ' || i, 'legacy-cat-' || i, '', NULL, \
                TIMESTAMP '2023-01-01 00:00:00' \
         FROM generate_series(1, {NUM_LEGACY_CATEGORIES}) AS i"
    ))
    .expect("seed legacy categories");
    conn.batch_execute(&format!(
        "INSERT INTO terms (taxonomy, name, slug, description, parent_id, created_at) \
         SELECT 'post_tag', 'Legacy Tag ' || i, 'legacy-tag-' || i, '', NULL, \
                TIMESTAMP '2023-01-01 00:00:00' \
         FROM generate_series(1, {NUM_LEGACY_TAGS}) AS i"
    ))
    .expect("seed legacy tags");

    // Real dead tuples: touch a slice of rows post-insert, same technique
    // every other Ledger fixture in this repo uses, no intervening VACUUM.
    conn.batch_execute("UPDATE terms SET description = 'reconciled' WHERE id % 7 = 0")
        .expect("create dead tuples");
    conn.batch_execute("ANALYZE terms").expect("analyze");
}

const NUM_TAGS: usize = 60;
const NUM_POSTS: usize = 600;
const TERMS_PER_POST: usize = 5;

/// Build the import file: a 60-tag `post_tag` vocabulary (declared in
/// `terms`, so the terms pass creates it before the posts pass runs), and
/// 600 draft posts each referencing 5 of those 60 tags — 3,000 term
/// references total, each tag reused roughly 50 times, the "heavily tagged
/// blog" shape.
fn build_export_payload() -> String {
    let mut terms = String::new();
    for tag in 0..NUM_TAGS {
        if !terms.is_empty() {
            terms.push(',');
        }
        terms.push_str(&format!(
            r#"{{"taxonomy":"post_tag","name":"Tag {tag}","slug":"tag-{tag}","description":"","parent":null}}"#
        ));
    }

    let mut posts = String::new();
    for post_index in 0..NUM_POSTS {
        if !posts.is_empty() {
            posts.push(',');
        }
        let mut post_terms = String::new();
        for offset in 0..TERMS_PER_POST {
            if !post_terms.is_empty() {
                post_terms.push(',');
            }
            let tag = (post_index * TERMS_PER_POST + offset) % NUM_TAGS;
            post_terms.push_str(&format!(r#"{{"taxonomy":"post_tag","slug":"tag-{tag}"}}"#));
        }
        posts.push_str(&format!(
            r#"{{"post_type":"post","title":"Post {post_index}","slug":"post-{post_index}","status":"draft","password":"","author":"owner","terms":[{post_terms}]}}"#
        ));
    }

    format!(
        r#"{{"version":5,"site_title":"Fixture Site","exported_at":"2026-09-14T00:00:00Z","terms":[{terms}],"posts":[{posts}],"attachments":[]}}"#
    )
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

/// Prints every `terms`-reading statement from this run and splits it into
/// the two shapes that matter:
///
/// - the single-row `"terms"."slug" = $1` shape, with no taxonomy predicate
///   in the SQL at all (the generated `TermRepository::find_by_slug`
///   filters only by slug; `resolve_import_terms` used to filter the
///   taxonomy in application code afterward) — the pre-fix per-(post, term)
///   lookup. 0 under the fix.
/// - the batched `"terms"."taxonomy" = $1) AND ("terms"."slug" = ANY(` shape
///   — the fix's one query per taxonomy, replacing every per-post call site
///   at once.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%FROM \"terms\"%' \
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
        } else if normalized.contains("\"slug\" = $") && !normalized.contains("\"taxonomy\" =") {
            single_calls += row.calls;
            single_buffers += row.buffers;
        }
    }
    println!(
        "-- unbatched per-reference slug-only lookup (find_by_slug, pre-fix): \
         calls={single_calls} buffers={single_buffers} -- batched taxonomy+slug ANY() \
         lookup (the fix): calls={any_calls} buffers={any_buffers} --"
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

#[derive(QueryableByName, Debug)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

fn count_table(conn: &mut PgConnection, table: &str) -> i64 {
    use diesel::RunQueryDsl;
    diesel::sql_query(format!("SELECT count(*) AS n FROM {table}"))
        .get_result::<CountRow>(conn)
        .unwrap_or_else(|e| panic!("count {table}: {e}"))
        .n
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn import_post_terms_resolve_batch_profile() {
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

    seed_background(&mut conn);
    let background_count = count_table(&mut conn, "terms");
    assert_eq!(
        background_count,
        NUM_LEGACY_CATEGORIES + NUM_LEGACY_TAGS,
        "fixture must seed exactly the legacy background before the import"
    );

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

    let cookie = register(&client, "owner").await;
    let payload = build_export_payload();

    // === Fresh restore: every one of the 600 posts is new ===
    reset_stats(&mut conn);
    let resp = import_export(&client, &cookie, &payload).await;
    assert!(
        resp.status.is_success(),
        "fresh import should succeed; body was: {}",
        resp.text()
    );
    let posts_count = count_table(&mut conn, "posts");
    assert_eq!(
        posts_count, NUM_POSTS as i64,
        "the fresh import must create exactly the file's 600 posts"
    );
    let post_terms_count = count_table(&mut conn, "post_terms");
    assert_eq!(
        post_terms_count,
        (NUM_POSTS * TERMS_PER_POST) as i64,
        "every post must be linked to exactly its 5 declared tags"
    );
    let fresh_profile = print_profile(&mut conn, "fresh restore (600 new posts)");

    // Equivalence: post-0's tags are exactly the ones the file names, in the
    // right taxonomy -- the batched resolver must not silently drop or
    // cross-wire a reference.
    #[derive(QueryableByName, Debug)]
    struct TagRow {
        #[diesel(sql_type = Text)]
        slug: String,
    }
    use diesel::RunQueryDsl;
    let post0_tags: Vec<TagRow> = diesel::sql_query(
        "SELECT t.slug AS slug FROM terms t \
         JOIN post_terms pt ON pt.term_id = t.id \
         JOIN posts p ON p.id = pt.post_id \
         WHERE p.slug = 'post-0' ORDER BY t.slug",
    )
    .load(&mut conn)
    .expect("post-0's tags");
    let mut expected: Vec<String> = (0..TERMS_PER_POST).map(|n| format!("tag-{n}")).collect();
    expected.sort();
    assert_eq!(
        post0_tags.into_iter().map(|r| r.slug).collect::<Vec<_>>(),
        expected,
        "post-0 must carry exactly its 5 declared tags, no more, no fewer"
    );

    // A reference to a tag the site (and the file) never declares must
    // resolve to nothing, not error -- same as the old per-row lookup
    // finding no match.
    let unknown_ref_resp = import_export(
        &client,
        &cookie,
        r#"{"version":5,"site_title":"Fixture Site","exported_at":"2026-09-14T00:00:00Z","terms":[],"posts":[{"post_type":"post","title":"Unknown Ref Post","slug":"unknown-ref-post","status":"draft","password":"","author":"owner","terms":[{"taxonomy":"post_tag","slug":"does-not-exist"}]}],"attachments":[]}"#,
    )
    .await;
    assert!(
        unknown_ref_resp.status.is_success(),
        "a post referencing an unresolvable term must still import, just untagged; body was: {}",
        unknown_ref_resp.text()
    );
    let unknown_ref_links = count_table(&mut conn, "post_terms");
    // Only the fresh-restore links from above, plus zero for this new post.
    #[derive(QueryableByName, Debug)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        n: i64,
    }
    let unknown_post_id =
        diesel::sql_query("SELECT id AS n FROM posts WHERE slug = 'unknown-ref-post'")
            .get_result::<IdRow>(&mut conn)
            .expect("unknown-ref post exists")
            .n;
    let unknown_post_links = diesel::sql_query(format!(
        "SELECT count(*) AS n FROM post_terms WHERE post_id = {unknown_post_id}"
    ))
    .get_result::<IdRow>(&mut conn)
    .expect("count links for unknown-ref post")
    .n;
    assert_eq!(
        unknown_post_links, 0,
        "a post referencing only an unresolvable term must import with zero term links"
    );
    assert_eq!(
        unknown_ref_links,
        (NUM_POSTS * TERMS_PER_POST) as i64,
        "the unresolvable reference must add no spurious link anywhere"
    );

    // Empty `terms: []` must also import cleanly (a post with no tags is the
    // common case for any non-blog post type).
    let empty_ref_resp = import_export(
        &client,
        &cookie,
        r#"{"version":5,"site_title":"Fixture Site","exported_at":"2026-09-14T00:00:00Z","terms":[],"posts":[{"post_type":"post","title":"No Tags Post","slug":"no-tags-post","status":"draft","password":"","author":"owner","terms":[]}],"attachments":[]}"#,
    )
    .await;
    assert!(
        empty_ref_resp.status.is_success(),
        "a post with an empty terms list must still import; body was: {}",
        empty_ref_resp.text()
    );

    println!("\n=== statement-count summary ===");
    println!(
        "{:<28} {:>12} {:>14} {:>12} {:>14}",
        "scenario", "single calls", "single buffers", "any calls", "any buffers"
    );
    let (label, p) = ("fresh restore (600 posts)", fresh_profile);
    println!(
        "{label:<28} {:>12} {:>14} {:>12} {:>14}",
        p.0, p.1, p.2, p.3
    );

    explain(
        &mut conn,
        "single-row slug-only lookup shape (find_by_slug, one per (post, term) reference, unbatched)",
        "SELECT * FROM terms WHERE slug = 'tag-30'",
    );
    let any_list = (0..NUM_TAGS)
        .map(|n| format!("'tag-{n}'"))
        .collect::<Vec<_>>()
        .join(",");
    explain(
        &mut conn,
        "batched taxonomy+slug ANY() lookup shape (the fix, one call for the whole post_tag vocabulary)",
        &format!(
            "SELECT * FROM terms WHERE taxonomy = 'post_tag' AND slug = ANY(ARRAY[{any_list}])"
        ),
    );
}

/// A reference many posts share but this site genuinely does not have (a
/// deprecated tag the destination dropped, still named by every post in an
/// old backup, is exactly this shape) must be re-checked against the live
/// table **at most once for the whole import**, not once per post that
/// names it -- `resolve_import_terms`'s post-review follow-up fix (the live
/// miss re-check that catches a term created mid-import) would otherwise
/// silently degrade back to the O(posts) per-reference round trip this
/// whole PR exists to remove, for exactly this shape.
///
/// 50 posts, each referencing one tag (`ghost-tag`) that is declared
/// nowhere in the file and does not exist on the site. The up-front batch
/// resolves nothing for it (1 call); the first post to see it miss spends
/// one live re-check (1 more call, also finding nothing) and every other
/// post reuses that same negative result. Total batched-lookup calls: 2,
/// never 51.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn import_post_terms_resolve_shared_miss_is_checked_once() {
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

    let cookie = register(&client, "owner").await;

    const NUM_POSTS: usize = 50;
    let mut posts = String::new();
    for post_index in 0..NUM_POSTS {
        if !posts.is_empty() {
            posts.push(',');
        }
        posts.push_str(&format!(
            r#"{{"post_type":"post","title":"Post {post_index}","slug":"post-{post_index}","status":"draft","password":"","author":"owner","terms":[{{"taxonomy":"post_tag","slug":"ghost-tag"}}]}}"#
        ));
    }
    let payload = format!(
        r#"{{"version":5,"site_title":"Fixture Site","exported_at":"2026-09-14T00:00:00Z","terms":[],"posts":[{posts}],"attachments":[]}}"#
    );

    reset_stats(&mut conn);
    let resp = import_export(&client, &cookie, &payload).await;
    assert!(
        resp.status.is_success(),
        "an import referencing a term this site does not have must still succeed, every post \
         just untagged; body was: {}",
        resp.text()
    );
    let posts_count = count_table(&mut conn, "posts");
    assert_eq!(
        posts_count, NUM_POSTS as i64,
        "every post must still import"
    );
    let links = count_table(&mut conn, "post_terms");
    assert_eq!(
        links, 0,
        "a term this site never has must leave every post untagged, no spurious link anywhere"
    );

    let (_, _, any_calls, _) = print_profile(
        &mut conn,
        "50 posts sharing one term this site does not have",
    );
    assert!(
        any_calls <= 2,
        "a reference every post shares but this site does not have must cost at most one \
         up-front batch call plus one live re-check for the whole import, not one per post \
         (got {any_calls} calls for {NUM_POSTS} posts) -- see resolve_import_terms's `checked` \
         negative cache"
    );
}
