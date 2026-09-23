//! Ledger profiling harness for `content::import_revisions` and
//! `content::import_post_meta` (`examples/cms/src/content.rs`), driven
//! through the real `POST /admin/tools/import` route — the same "Import"
//! button handler `import_terms_batch_profile.rs` exercises, restoring a
//! site-export JSON file (the `Export`/`ExportPost` payload shape).
//!
//! Before the fix in this same PR, both functions insert their per-post rows
//! one at a time inside a `for` loop: `import_revisions` does one
//! `INSERT INTO revisions` per kept revision (up to `REVISION_LIMIT` — 25 —
//! per post), and `import_post_meta` does one `INSERT INTO post_meta` per
//! custom field (unbounded per post; a plugin-heavy WordPress export
//! routinely carries dozens). Both are the exact shape `import_terms` had
//! before #2791 batched it — sequential single-row `INSERT`s inside the same
//! per-post import loop `tools.rs::import` runs once per file — except
//! neither of these two siblings was fixed then.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p cms --test import_revisions_meta_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! A single import file carrying 300 posts, each with `REVISION_LIMIT` (25)
//! revisions and 18 custom fields — a heavily-edited, plugin-instrumented
//! blog's backup, not a toy. 300 is small enough to build and import inside
//! one test run, large enough that the per-post loop's shape (not its
//! constant overhead) dominates the statement count: 7,500 individual
//! revision inserts and 5,400 individual meta inserts pre-fix, against 300
//! and 300 batched multi-row inserts post-fix.
//!
//! Every post lands as `status: "draft"` so the import performs no status
//! transition (`import_status` only transitions when the file's status is
//! not `"draft"`), keeping the profiled statement counts to exactly the two
//! functions under test rather than mixed with `transition_status`'s own
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

const NUM_POSTS: usize = 300;
const REVISIONS_PER_POST: usize = 25; // == content::REVISION_LIMIT
const META_FIELDS_PER_POST: usize = 18;

/// Build the import file: `NUM_POSTS` draft posts, each carrying
/// `REVISIONS_PER_POST` revisions and `META_FIELDS_PER_POST` custom fields.
/// No terms, no comments — isolates the two functions under test from the
/// already-batched `import_terms` and from `import_comments`'s own
/// (structurally different, parent-id-dependent) insert loop.
fn build_export_payload() -> String {
    let mut posts = String::new();
    for post_index in 0..NUM_POSTS {
        if !posts.is_empty() {
            posts.push(',');
        }
        let mut revisions = String::new();
        for rev in 0..REVISIONS_PER_POST {
            if !revisions.is_empty() {
                revisions.push(',');
            }
            revisions.push_str(&format!(
                r#"{{"author":"owner","title":"Revision {rev} of post {post_index}","excerpt":"","body":"body text for revision {rev}","status":"draft","summary":"edit {rev}","created_at":"2026-01-01T00:00:{rev:02}"}}"#
            ));
        }
        let mut meta = String::new();
        for field in 0..META_FIELDS_PER_POST {
            if !meta.is_empty() {
                meta.push(',');
            }
            meta.push_str(&format!(
                r#"{{"key":"seo_field_{field}","value":"value-{post_index}-{field}"}}"#
            ));
        }
        posts.push_str(&format!(
            r#"{{"post_type":"post","title":"Post {post_index}","slug":"post-{post_index}","status":"draft","password":"","author":"owner","revisions":[{revisions}],"meta":[{meta}]}}"#
        ));
    }
    format!(
        r#"{{"version":5,"site_title":"Fixture Site","exported_at":"2026-09-14T00:00:00Z","terms":[],"posts":[{posts}],"attachments":[]}}"#
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

/// Prints every `revisions`/`post_meta`-touching statement from this run,
/// split by shape: a single-row `INSERT ... VALUES ($1, $2, ...)` vs. a
/// multi-row `INSERT ... VALUES ($1, ...), ($n, ...), ...`.
/// `pg_stat_statements` normalizes a multi-row `INSERT`'s `VALUES` list into
/// one entry regardless of row count, so the two shapes are textually
/// distinguishable by how many `($`-prefixed row groups the normalized query
/// carries — counted here directly, not approximated.
///
/// `post_meta` also receives two importer-owned marker writes this harness
/// does not touch: `record_import_source` (one single-row insert per post —
/// same statement shape as the pre-fix per-field loop, so it merges into the
/// same `pg_stat_statements` entry and inflates the single-row bucket's call
/// count by `NUM_POSTS` on both sides of the fix) and `mark_imports_complete`
/// (already one batched multi-row insert for the whole run, unaffected by
/// this fix either way). `revisions` carries no such marker, so its numbers
/// are attributable to `import_revisions` alone.
fn print_profile(conn: &mut PgConnection, label: &str) -> ImportProfile {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE (query ILIKE '%INSERT INTO \"revisions\"%' \
                OR query ILIKE '%INSERT INTO \"post_meta\"%') \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let mut profile = ImportProfile::default();
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        // Row-group count: how many `(` a `VALUES` clause opens, which is
        // exactly the number of rows one call of this statement inserts.
        let row_groups = normalized.matches("($").count().max(1);
        println!(
            "calls={:<6} buffers={:<8} rows/call={row_groups:<3} {normalized}",
            row.calls, row.buffers
        );
        let is_revisions = normalized.starts_with("INSERT INTO \"revisions\"");
        let is_meta = normalized.starts_with("INSERT INTO \"post_meta\"");
        let multi_row = row_groups > 1;
        if is_revisions {
            if multi_row {
                profile.revisions_multi_calls += row.calls;
                profile.revisions_multi_buffers += row.buffers;
            } else {
                profile.revisions_single_calls += row.calls;
                profile.revisions_single_buffers += row.buffers;
            }
        } else if is_meta {
            if multi_row {
                profile.meta_multi_calls += row.calls;
                profile.meta_multi_buffers += row.buffers;
            } else {
                profile.meta_single_calls += row.calls;
                profile.meta_single_buffers += row.buffers;
            }
        }
    }
    println!(
        "-- revisions: single-row calls={} buffers={}, multi-row calls={} buffers={} \
         -- post_meta: single-row calls={} buffers={} (includes {NUM_POSTS} \
         record_import_source marker writes), multi-row calls={} buffers={} --",
        profile.revisions_single_calls,
        profile.revisions_single_buffers,
        profile.revisions_multi_calls,
        profile.revisions_multi_buffers,
        profile.meta_single_calls,
        profile.meta_single_buffers,
        profile.meta_multi_calls,
        profile.meta_multi_buffers,
    );
    profile
}

#[derive(Default, Debug, Clone, Copy)]
struct ImportProfile {
    revisions_single_calls: i64,
    revisions_single_buffers: i64,
    revisions_multi_calls: i64,
    revisions_multi_buffers: i64,
    meta_single_calls: i64,
    meta_single_buffers: i64,
    meta_multi_calls: i64,
    meta_multi_buffers: i64,
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
async fn import_revisions_meta_batch_profile() {
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
    let payload = build_export_payload();

    reset_stats(&mut conn);
    let resp = import_export(&client, &cookie, &payload).await;
    assert!(
        resp.status.is_success(),
        "import should succeed; body was: {}",
        resp.text()
    );

    let revisions_count = count_table(&mut conn, "revisions");
    let meta_count = count_table(&mut conn, "post_meta");
    assert_eq!(
        revisions_count,
        (NUM_POSTS * REVISIONS_PER_POST) as i64,
        "every post's revisions must be restored"
    );
    // The file's own fields, plus the importer's two per-post marker rows:
    // `record_import_source`'s `_import_source_slug` (written per post, right
    // after its insert) and `mark_imports_complete`'s `_import_completed`
    // (written once at the end of the run, for every post it just finished).
    assert_eq!(
        meta_count,
        (NUM_POSTS * (META_FIELDS_PER_POST + 2)) as i64,
        "every post's custom fields, plus the importer's own two marker rows per post, must be present"
    );

    // Result equivalence, not just counts: the batched multi-row INSERT must
    // write the exact same values, in the exact same order, as the sequential
    // single-row loop it replaces. Spot-checked on one post (`post-0`) rather
    // than all 300, since every post is built by the same code path from the
    // same template — a scrambled batch would show up here identically to how
    // it would show up on any other post.
    #[derive(QueryableByName, Debug)]
    struct RevisionRow {
        #[diesel(sql_type = Text)]
        title: String,
        #[diesel(sql_type = Text)]
        summary: String,
    }
    use diesel::RunQueryDsl;
    let sample_revisions: Vec<RevisionRow> = diesel::sql_query(
        "SELECT r.title AS title, r.summary AS summary FROM revisions r \
         JOIN posts p ON p.id = r.post_id \
         WHERE p.slug = 'post-0' ORDER BY r.created_at ASC, r.id ASC",
    )
    .load(&mut conn)
    .expect("sample revisions for post-0");
    assert_eq!(
        sample_revisions.len(),
        REVISIONS_PER_POST,
        "post-0 must carry every one of its revisions"
    );
    for (index, row) in sample_revisions.iter().enumerate() {
        assert_eq!(row.title, format!("Revision {index} of post 0"));
        assert_eq!(row.summary, format!("edit {index}"));
    }

    #[derive(QueryableByName, Debug)]
    struct MetaRow {
        #[diesel(sql_type = Text)]
        meta_key: String,
        #[diesel(sql_type = Text)]
        meta_value: String,
    }
    let sample_meta: Vec<MetaRow> = diesel::sql_query(
        "SELECT meta_key, meta_value FROM post_meta m \
         JOIN posts p ON p.id = m.post_id \
         WHERE p.slug = 'post-0' AND meta_key LIKE 'seo_field_%'",
    )
    .load(&mut conn)
    .expect("sample meta for post-0");
    assert_eq!(
        sample_meta.len(),
        META_FIELDS_PER_POST,
        "post-0 must carry every one of its custom fields (marker keys excluded)"
    );
    let by_key: std::collections::HashMap<String, String> = sample_meta
        .into_iter()
        .map(|row| (row.meta_key, row.meta_value))
        .collect();
    for field in 0..META_FIELDS_PER_POST {
        let key = format!("seo_field_{field}");
        assert_eq!(
            by_key.get(&key).map(String::as_str),
            Some(format!("value-0-{field}").as_str()),
            "field {key} must round-trip its exact value"
        );
    }

    let profile = print_profile(&mut conn, "fresh restore (300 posts)");

    println!("\n=== statement-count summary ===");
    println!(
        "{:<28} {:>11} {:>13} {:>11} {:>13} {:>11} {:>13}",
        "scenario", "rev 1-row", "rev 1-buf", "rev N-row", "rev N-buf", "meta 1-row", "meta 1-buf"
    );
    println!(
        "{:<28} {:>11} {:>13} {:>11} {:>13} {:>11} {:>13}",
        "fresh restore (300 posts)",
        profile.revisions_single_calls,
        profile.revisions_single_buffers,
        profile.revisions_multi_calls,
        profile.revisions_multi_buffers,
        profile.meta_single_calls,
        profile.meta_single_buffers,
    );
    println!(
        "meta multi-row insert: calls={} buffers={}",
        profile.meta_multi_calls, profile.meta_multi_buffers
    );

    explain(
        &mut conn,
        "single-row revision insert shape (pre-fix, one round trip per revision)",
        "INSERT INTO revisions (post_id, title, excerpt, body, status, summary, created_at) \
         VALUES (1, 'x', '', '', 'draft', '', now())",
    );
    let rows: Vec<String> = (0..REVISIONS_PER_POST)
        .map(|n| format!("(1, 'x{n}', '', '', 'draft', '', now())"))
        .collect();
    explain(
        &mut conn,
        "multi-row revision insert shape (the fix, one call for all of one post's revisions)",
        &format!(
            "INSERT INTO revisions (post_id, title, excerpt, body, status, summary, created_at) \
             VALUES {}",
            rows.join(", ")
        ),
    );
}
