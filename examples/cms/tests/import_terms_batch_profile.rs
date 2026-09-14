//! Ledger profiling harness for `content::import_terms`
//! (`examples/cms/src/content.rs`), driven through the real
//! `POST /admin/tools/import` route — the handler behind the "Import" button
//! on `/admin/tools`, which restores a site-export JSON file (its own
//! `Export`/`ExportTerm` payload shape) as posts, terms and media.
//!
//! Before the fix in this same PR, `import_terms` walks the file's terms in
//! two sequential `for` loops, each doing one or two single-row Diesel
//! `.first()` calls per incoming term (`content.rs`, existence check ~4573,
//! child/parent lookup ~4614/4621), then inserts each new row with its own
//! `INSERT ... RETURNING` (~4582). A taxonomy backup with heavy tagging —
//! ordinary for a blog that has tagged posts individually for years — pays
//! for this once per term: up to three round trips per row, none of them
//! grouped, on a request whose entire cost should be a handful of statements
//! per distinct taxonomy (`category`, `post_tag`, ...), not per term.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p cms --test import_terms_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! 5,000 pre-existing `terms` rows (2,500 `category`, 2,500 `post_tag`, slugs
//! prefixed `legacy-`) as "the site's existing taxonomy before the restore" —
//! large enough that the unique `(taxonomy, slug)` index this call leans on
//! has a realistic size rather than trivially fitting in one page. Real dead
//! tuples from a follow-up `UPDATE` before `ANALYZE`, no `VACUUM`, matching
//! every other Ledger fixture in this repo.
//!
//! The import file itself carries 2,000 terms: a 2-level `category` tree (8
//! top-level categories, each with 4 children — 40 rows) and 1,960 flat
//! `post_tag` rows, the shape a heavily-tagged blog's backup actually has.
//! Every child category is listed **before** its parent in the file, the
//! case `import_terms`'s own doc comment calls out ("a child term can appear
//! in the file before its parent") and the reason the function does two
//! passes at all.
//!
//! The same 2,000-term file is imported **twice**: once against a site that
//! has none of it yet (every row new, exercising the insert + parent-link
//! passes), and a second time, unchanged, against the site that first import
//! just produced (every row already present, exercising only the
//! existence-check pass) — the "someone re-uploads the same backup" case,
//! and the case that isolates the pure-read half of the defect from the
//! writes.

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
        resp.status, 303,
        "registration should redirect; body was: {}",
        resp.text()
    );
    session_cookie(&resp)
}

/// Upload an export file to the importer. Mirrors
/// `tests/integration_test.rs`'s `import_export`.
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

const NUM_LEGACY_CATEGORIES: i64 = 2_500;
const NUM_LEGACY_TAGS: i64 = 2_500;

/// The site's taxonomy before the restore: 5,000 unrelated pre-existing
/// terms, so the unique `(taxonomy, slug)` index this call leans on has a
/// realistic size instead of trivially fitting on one page.
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

const NUM_TOP_CATEGORIES: usize = 8;
const CHILDREN_PER_CATEGORY: usize = 4;
const NUM_TAGS: usize = 1_960;

/// Build the 2,000-term import file: an 8-top-level/4-children-each category
/// tree (child rows listed before their parent, the ordering
/// `import_terms`'s two-pass design exists to handle) plus 1,960 flat tags.
fn build_export_payload() -> String {
    let mut terms = String::new();
    for top in 0..NUM_TOP_CATEGORIES {
        for child in 0..CHILDREN_PER_CATEGORY {
            if !terms.is_empty() {
                terms.push(',');
            }
            terms.push_str(&format!(
                r#"{{"taxonomy":"category","name":"Category {top}-{child}","slug":"cat-{top}-{child}","description":"","parent":"cat-{top}"}}"#
            ));
        }
        if !terms.is_empty() {
            terms.push(',');
        }
        terms.push_str(&format!(
            r#"{{"taxonomy":"category","name":"Category {top}","slug":"cat-{top}","description":"","parent":null}}"#
        ));
    }
    for tag in 0..NUM_TAGS {
        terms.push(',');
        terms.push_str(&format!(
            r#"{{"taxonomy":"post_tag","name":"Tag {tag}","slug":"tag-{tag}","description":"","parent":null}}"#
        ));
    }
    format!(
        r#"{{"version":5,"site_title":"Fixture Site","exported_at":"2026-09-14T00:00:00Z","terms":[{terms}],"posts":[],"attachments":[]}}"#
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

/// Prints every `terms`-touching statement from this run and splits it into
/// the three shapes that matter:
///
/// - the single-row `"taxonomy" = $1) AND ("terms"."slug" = $2` shape (no
///   `ANY(`) — the pre-fix existence check *and* the pre-fix child/parent
///   lookup share this exact normalized text, so `pg_stat_statements`
///   aggregates all three call sites under one row. 100% attributable to the
///   unbatched defect: it is 0 under the fix.
/// - the batched `"slug" = ANY($` shape — the fix's one query per taxonomy
///   chunk, replacing all three unbatched call sites at once.
/// - `INSERT INTO "terms"` — one row per incoming term pre-fix, one row per
///   1,000-term chunk post-fix.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%terms%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut single_calls, mut single_buffers) = (0i64, 0i64);
    let (mut any_calls, mut any_buffers) = (0i64, 0i64);
    let (mut insert_calls, mut insert_buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        if normalized.starts_with("INSERT INTO \"terms\"") {
            insert_calls += row.calls;
            insert_buffers += row.buffers;
        } else if normalized.contains("\"slug\" = ANY(") {
            any_calls += row.calls;
            any_buffers += row.buffers;
        } else if normalized.contains("\"slug\" = $") {
            single_calls += row.calls;
            single_buffers += row.buffers;
        }
    }
    println!(
        "-- single-row taxonomy+slug lookup (unbatched existence/child/parent checks): \
         calls={single_calls} buffers={single_buffers} -- batched taxonomy+slug ANY() lookup \
         (the fix): calls={any_calls} buffers={any_buffers} -- INSERT INTO terms: \
         calls={insert_calls} buffers={insert_buffers} --"
    );
    (
        single_calls,
        single_buffers,
        any_calls,
        any_buffers,
        insert_calls,
        insert_buffers,
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

#[derive(QueryableByName, Debug)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

fn count_terms(conn: &mut PgConnection) -> i64 {
    use diesel::RunQueryDsl;
    diesel::sql_query("SELECT count(*) AS n FROM terms")
        .get_result::<CountRow>(conn)
        .expect("count terms")
        .n
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn import_terms_batch_profile() {
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
    let background_count = count_terms(&mut conn);
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

    // === Fresh restore: every one of the file's 2,000 terms is new ===
    reset_stats(&mut conn);
    let resp = import_export(&client, &cookie, &payload).await;
    assert!(
        resp.status.is_success(),
        "fresh import should succeed; body was: {}",
        resp.text()
    );
    let after_fresh = count_terms(&mut conn);
    assert_eq!(
        after_fresh,
        background_count + (NUM_TOP_CATEGORIES * (CHILDREN_PER_CATEGORY + 1)) as i64
            + NUM_TAGS as i64,
        "the fresh import must create exactly the file's 2,000 new terms"
    );
    let fresh_profile = print_profile(&mut conn, "fresh restore (2,000 new terms)");

    // Equivalence: every child category actually got linked under its
    // parent -- the batched rewrite must not silently drop what the
    // two-pass design exists to do.
    #[derive(QueryableByName, Debug)]
    struct LinkRow {
        #[diesel(sql_type = Text)]
        child_slug: String,
        #[diesel(sql_type = diesel::sql_types::Bool)]
        linked: bool,
    }
    use diesel::RunQueryDsl;
    let links = diesel::sql_query(
        "SELECT c.slug AS child_slug, (c.parent_id = p.id) AS linked \
         FROM terms c JOIN terms p \
           ON p.taxonomy = 'category' AND p.slug = 'cat-' || split_part(c.slug, '-', 2) \
         WHERE c.taxonomy = 'category' AND c.slug LIKE 'cat-%-%' \
         ORDER BY c.slug",
    )
    .load::<LinkRow>(&mut conn)
    .expect("check parent links");
    assert_eq!(
        links.len(),
        NUM_TOP_CATEGORIES * CHILDREN_PER_CATEGORY,
        "every child category must be found under its expected parent row"
    );
    for link in &links {
        assert!(link.linked, "{} must be linked to its parent", link.child_slug);
    }

    // === Idempotent reimport: the same file, now entirely already present ===
    reset_stats(&mut conn);
    let resp = import_export(&client, &cookie, &payload).await;
    assert!(
        resp.status.is_success(),
        "reimport should succeed; body was: {}",
        resp.text()
    );
    let after_reimport = count_terms(&mut conn);
    assert_eq!(
        after_reimport, after_fresh,
        "reimporting the same file must create zero additional rows"
    );
    let reimport_profile = print_profile(&mut conn, "idempotent reimport (0 new terms)");

    println!("\n=== statement-count summary ===");
    println!(
        "{:<20} {:>12} {:>14} {:>12} {:>14} {:>10} {:>12}",
        "scenario",
        "single calls",
        "single buffers",
        "any calls",
        "any buffers",
        "ins calls",
        "ins buffers"
    );
    for (label, p) in [("fresh restore", fresh_profile), ("reimport", reimport_profile)] {
        println!(
            "{label:<20} {:>12} {:>14} {:>12} {:>14} {:>10} {:>12}",
            p.0, p.1, p.2, p.3, p.4, p.5
        );
    }

    explain(
        &mut conn,
        "single-row taxonomy+slug lookup shape (existence/child/parent check, one per term, unbatched)",
        "SELECT * FROM terms WHERE taxonomy = 'post_tag' AND slug = 'legacy-tag-1000'",
    );
    let any_list = (0..NUM_TAGS)
        .map(|n| format!("'tag-{n}'"))
        .collect::<Vec<_>>()
        .join(",");
    explain(
        &mut conn,
        "batched taxonomy+slug ANY() lookup shape (the fix, one call for the whole post_tag chunk)",
        &format!("SELECT * FROM terms WHERE taxonomy = 'post_tag' AND slug = ANY(ARRAY[{any_list}])"),
    );
}
