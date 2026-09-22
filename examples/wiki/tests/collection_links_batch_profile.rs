//! Ledger profiling harness for `routes::collections::{create, update}`
//! (`examples/wiki/src/routes/collections.rs`), driven through the real
//! `POST /collections` and `POST /collections/{id}` routes — the handlers
//! behind the "Create collection" / "Save changes" buttons on the nested
//! (`has_many`) collection-links form (`docs/guide/nested-forms.md`).
//!
//! Before the fix in this same PR, both handlers walk the submitted `links`
//! Vec in a `for` loop and issue one `INSERT INTO collection_links ...`
//! per row (`create` ~line 216, `update` ~line 341) inside the single
//! transaction that also inserts/updates the parent `collections` row. A
//! collection is, by its own migration comment, "a curated set of related
//! external links" — a resource page, a reading list — so a real one can
//! carry dozens to hundreds of links; each save of it pays one round trip
//! per link instead of the one multi-row `INSERT ... VALUES (...), (...), ...`
//! it should be.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p wiki --test collection_links_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! ## Fixture
//!
//! 150 pre-existing `collections` rows, 12 links each (1,800 background
//! `collection_links` rows) — "the site's existing collections before this
//! save" — large enough that `idx_collection_links_collection_id` has a
//! realistic size instead of trivially fitting on one page. Real dead
//! tuples from a follow-up `UPDATE` before `ANALYZE`, no `VACUUM`, matching
//! every other Ledger fixture in this repo.
//!
//! The two profiled requests each carry 200 links — plausible for a
//! "developer resources" or "further reading" page accumulated over years —
//! against a collection that is *not* part of the background set, so its
//! own row and its links are cleanly attributable in the profile. A third,
//! separate scenario submits 2,500 links to exercise the fix's chunking
//! (one multi-row `INSERT` per 1,000 rows, `MAX_BIND_PARAMS`-bounded) across
//! more than one chunk, per review feedback on the original single
//! unbounded `INSERT` (autumn#2888).

#![allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]

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

const MIGRATION_FILES: &[&str] = &[
    include_str!("../migrations/00000000000000_create_wiki/up.sql"),
    include_str!("../migrations/20260506000000_add_lock_version_to_pages/up.sql"),
    include_str!("../migrations/20260524000000_add_search_to_pages/up.sql"),
    include_str!("../migrations/20260601000000_add_api_credentials/up.sql"),
    include_str!("../migrations/20260721000000_add_collections/up.sql"),
];

fn apply_migrations(conn: &mut PgConnection) {
    for sql in MIGRATION_FILES {
        conn.batch_execute(sql).expect("apply wiki migration");
    }
}

/// URL-encode a form field value. Mirrors `examples/cms/tests/*`'s `encode`.
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

/// Build a `NestedChangesetForm<CollectionForm, LinkForm>` submission body:
/// `title=...&links[0][label]=...&links[0][url]=...&links[1][label]=...`
fn collection_form_body(title: &str, links: &[(String, String)]) -> String {
    let mut body = format!("title={}", encode(title));
    for (i, (label, url)) in links.iter().enumerate() {
        body.push_str(&format!(
            "&links[{i}][label]={}&links[{i}][url]={}",
            encode(label),
            encode(url)
        ));
    }
    body
}

/// `count` links shaped like a real curated list: `("Resource {i}", "https://example.org/resource-{i}")`.
fn make_links(prefix: &str, count: usize) -> Vec<(String, String)> {
    (0..count)
        .map(|i| {
            (
                format!("{prefix} {i}"),
                format!(
                    "https://example.org/{}/{i}",
                    prefix.to_lowercase().replace(' ', "-")
                ),
            )
        })
        .collect()
}

const NUM_LEGACY_COLLECTIONS: i64 = 150;
const LINKS_PER_LEGACY_COLLECTION: i64 = 12;

/// The site's existing collections before this save: 150 unrelated
/// collections, 12 links each, so `idx_collection_links_collection_id` has a
/// realistic size instead of trivially fitting on one page.
fn seed_background(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO collections (title, created_at) \
         SELECT 'Legacy Collection ' || i, TIMESTAMP '2023-01-01 00:00:00' \
         FROM generate_series(1, {NUM_LEGACY_COLLECTIONS}) AS i"
    ))
    .expect("seed legacy collections");
    conn.batch_execute(&format!(
        "INSERT INTO collection_links (collection_id, label, url, position, created_at) \
         SELECT c.id, 'Legacy Link ' || g, 'https://example.com/legacy/' || c.id || '/' || g, \
                (g - 1)::int, TIMESTAMP '2023-01-01 00:00:00' \
         FROM collections c CROSS JOIN generate_series(1, {LINKS_PER_LEGACY_COLLECTION}) AS g \
         WHERE c.title LIKE 'Legacy Collection %'"
    ))
    .expect("seed legacy links");

    // Real dead tuples: touch a slice of rows post-insert, same technique
    // every other Ledger fixture in this repo uses, no intervening VACUUM.
    conn.batch_execute(
        "UPDATE collection_links SET label = label || ' (touched)' WHERE id % 7 = 0",
    )
    .expect("create dead tuples");
    conn.batch_execute("ANALYZE collections")
        .expect("analyze collections");
    conn.batch_execute("ANALYZE collection_links")
        .expect("analyze collection_links");
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

/// Prints every `collection_links`-touching statement from this run and
/// splits it into the shape that matters: `INSERT INTO "collection_links"`
/// (one row per link pre-fix, one call total post-fix) vs. everything else
/// (the parent-row work and, for `update`, the one `DELETE`).
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64, i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%collection_links%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut insert_calls, mut insert_buffers) = (0i64, 0i64);
    let (mut other_calls, mut other_buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        if normalized.starts_with("INSERT INTO \"collection_links\"") {
            insert_calls += row.calls;
            insert_buffers += row.buffers;
        } else {
            other_calls += row.calls;
            other_buffers += row.buffers;
        }
    }
    println!(
        "-- INSERT INTO collection_links: calls={insert_calls} buffers={insert_buffers} \
         -- other collection_links statements (DELETE, etc.): calls={other_calls} \
         buffers={other_buffers} --"
    );
    (insert_calls, insert_buffers, other_calls, other_buffers)
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

/// `label`, `url`, `position` for every link of a collection, ordered by
/// `position` — the deterministic tiebreaker a render/read always sorts on
/// (`load_links` in `routes/collections.rs`), so this is the row shape and
/// order to compare for the result-equivalence check.
#[derive(QueryableByName, Debug, PartialEq, Eq)]
struct LinkRow {
    #[diesel(sql_type = Text)]
    label: String,
    #[diesel(sql_type = Text)]
    url: String,
    #[diesel(sql_type = diesel::sql_types::Integer)]
    position: i32,
}

fn links_for(conn: &mut PgConnection, collection_id: i64) -> Vec<LinkRow> {
    use diesel::RunQueryDsl;
    diesel::sql_query(
        "SELECT label, url, position FROM collection_links \
         WHERE collection_id = $1 ORDER BY position",
    )
    .bind::<BigInt, _>(collection_id)
    .load(conn)
    .expect("load links")
}

/// Reference implementation of the **pre-fix** insert loop: one
/// `INSERT ... RETURNING` per link, exactly what `create`/`update` did
/// before this PR. Used only to build an independent, differently-coded
/// collection for the equivalence check below — the route under test
/// always runs the (now-batched) real handler.
fn insert_links_one_by_one(
    conn: &mut PgConnection,
    collection_id: i64,
    links: &[(String, String)],
) {
    use diesel::RunQueryDsl;
    for (i, (label, url)) in links.iter().enumerate() {
        diesel::sql_query(
            "INSERT INTO collection_links (collection_id, label, url, position) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind::<BigInt, _>(collection_id)
        .bind::<Text, _>(label)
        .bind::<Text, _>(url)
        .bind::<diesel::sql_types::Integer, _>(i as i32)
        .execute(conn)
        .expect("reference insert");
    }
}

fn insert_collection(conn: &mut PgConnection, title: &str) -> i64 {
    use diesel::RunQueryDsl;
    #[derive(QueryableByName, Debug)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
    diesel::sql_query("INSERT INTO collections (title) VALUES ($1) RETURNING id")
        .bind::<Text, _>(title)
        .get_result::<IdRow>(conn)
        .expect("insert collection")
        .id
}

async fn post_collection_form(client: &TestClient, path: &str, body: &str) -> TestResponse {
    client.post(path).form(body).send().await
}

fn new_collection_id(conn: &mut PgConnection, title: &str) -> i64 {
    use diesel::RunQueryDsl;
    #[derive(QueryableByName, Debug)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
    diesel::sql_query("SELECT id FROM collections WHERE title = $1")
        .bind::<Text, _>(title)
        .get_result::<IdRow>(conn)
        .expect("find created collection")
        .id
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn collection_links_batch_profile() {
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
    apply_migrations(&mut conn);

    seed_background(&mut conn);

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(config).build().expect("pool");

    let client: TestClient = TestApp::new()
        .routes(wiki::all_routes())
        .with_db(pool)
        .build();

    const NUM_LINKS: usize = 200;

    // === create: POST /collections with 200 links ===
    let create_links = make_links("Resource", NUM_LINKS);
    let create_body = collection_form_body("Fixture Collection", &create_links);

    reset_stats(&mut conn);
    let resp = post_collection_form(&client, "/collections", &create_body).await;
    assert_eq!(
        resp.status,
        303,
        "a valid create should redirect; body was: {}",
        resp.text()
    );
    let create_profile = print_profile(&mut conn, "create (200 new links)");

    let collection_id = new_collection_id(&mut conn, "Fixture Collection");
    let persisted = links_for(&mut conn, collection_id);
    let expected: Vec<LinkRow> = create_links
        .iter()
        .enumerate()
        .map(|(i, (label, url))| LinkRow {
            label: label.clone(),
            url: url.clone(),
            position: i as i32,
        })
        .collect();
    assert_eq!(
        persisted, expected,
        "create must persist every submitted link, in submitted order, unchanged \
         by batching the insert"
    );

    // === update: POST /collections/{id}, replacing the set with 200 different links ===
    let update_links = make_links("Updated", NUM_LINKS);
    let update_body = collection_form_body("Fixture Collection (edited)", &update_links);

    reset_stats(&mut conn);
    let resp = post_collection_form(
        &client,
        &format!("/collections/{collection_id}"),
        &update_body,
    )
    .await;
    assert_eq!(
        resp.status,
        303,
        "a valid update should redirect; body was: {}",
        resp.text()
    );
    let update_profile = print_profile(&mut conn, "update (replace with 200 different links)");

    let persisted = links_for(&mut conn, collection_id);
    let expected: Vec<LinkRow> = update_links
        .iter()
        .enumerate()
        .map(|(i, (label, url))| LinkRow {
            label: label.clone(),
            url: url.clone(),
            position: i as i32,
        })
        .collect();
    assert_eq!(
        persisted, expected,
        "update must replace the link set with exactly what was submitted, in \
         submitted order, and must not resurrect any link from the deleted set"
    );

    // === edge cases the batched insert must not break ===
    // Empty set: no `INSERT ... VALUES ()` must ever be attempted.
    let empty_body = collection_form_body("Empty Collection", &[]);
    let resp = post_collection_form(&client, "/collections", &empty_body).await;
    assert_eq!(
        resp.status, 303,
        "an empty link set must still create the parent"
    );
    let empty_id = new_collection_id(&mut conn, "Empty Collection");
    assert!(
        links_for(&mut conn, empty_id).is_empty(),
        "an empty submission must persist zero links, not error or leave a stray row"
    );

    // Duplicate content: collection_links has no uniqueness constraint beyond
    // `id`, so two rows with identical label+url are legal and must both
    // survive the batched insert (not be silently deduplicated).
    let dup = vec![
        ("Same Link".to_owned(), "https://example.org/dup".to_owned()),
        ("Same Link".to_owned(), "https://example.org/dup".to_owned()),
    ];
    let dup_body = collection_form_body("Duplicate Collection", &dup);
    let resp = post_collection_form(&client, "/collections", &dup_body).await;
    assert_eq!(
        resp.status, 303,
        "a duplicate-content link set must still create"
    );
    let dup_id = new_collection_id(&mut conn, "Duplicate Collection");
    let dup_persisted = links_for(&mut conn, dup_id);
    assert_eq!(
        dup_persisted.len(),
        2,
        "both duplicate-content rows must be persisted, not deduplicated"
    );

    // Bind-parameter chunking: a single un-chunked `INSERT ... VALUES` for
    // this table needs 4 bind params per row, so 16,384+ links in one
    // statement would exceed Postgres's 65,535-parameter limit — well within
    // what the default request-body-size limit allows through. 2,500 links
    // crosses the fix's 1,000-row chunk boundary (twice), so this exercises
    // >1 chunk without needing anywhere near the actual bind-param ceiling.
    const NUM_LARGE_LINKS: usize = 2_500;
    let large_links = make_links("Bulk", NUM_LARGE_LINKS);
    let large_body = collection_form_body("Bulk Collection", &large_links);
    reset_stats(&mut conn);
    let resp = post_collection_form(&client, "/collections", &large_body).await;
    assert_eq!(
        resp.status,
        303,
        "a 2,500-link submission must still succeed; body was: {}",
        resp.text()
    );
    let large_profile = print_profile(&mut conn, "create (2,500 links, chunked insert)");
    assert_eq!(
        large_profile.0, 3,
        "2,500 links at a 1,000-row chunk size must issue exactly 3 INSERT \
         statements (1,000 + 1,000 + 500) — 1 would risk a bind-parameter \
         overflow at real scale, 2,500 would mean chunking regressed to \
         one-per-row"
    );
    let large_id = new_collection_id(&mut conn, "Bulk Collection");
    let large_persisted = links_for(&mut conn, large_id);
    assert_eq!(
        large_persisted,
        expected_for(&large_links),
        "chunked insert must persist every link, across chunk boundaries, in order"
    );

    // === independent equivalence check: batched route output vs. a
    // one-by-one reference insert, same input, same table ===
    let reference_id = insert_collection(&mut conn, "Reference Collection");
    insert_links_one_by_one(&mut conn, reference_id, &create_links);
    let reference_rows = links_for(&mut conn, reference_id);
    assert_eq!(
        reference_rows,
        expected_for(&create_links),
        "sanity: the reference one-by-one helper must itself persist the input faithfully"
    );
    // Re-fetch the batched `create` collection's rows (already asserted equal
    // to `expected` above, before the update ran) is no longer possible post
    // update; compare the *update* scenario's batched output against a fresh
    // one-by-one insert of the same `update_links` input instead.
    let reference_update_id = insert_collection(&mut conn, "Reference Collection (update shape)");
    insert_links_one_by_one(&mut conn, reference_update_id, &update_links);
    let reference_update_rows = links_for(&mut conn, reference_update_id);
    let batched_update_rows = links_for(&mut conn, collection_id);
    assert_eq!(
        reference_update_rows, batched_update_rows,
        "the batched insert must persist byte-identical rows (label, url, position) \
         to the pre-fix one-by-one loop for the same input"
    );

    println!("\n=== statement-count summary ===");
    println!(
        "{:<12} {:>12} {:>14} {:>12} {:>14}",
        "scenario", "ins calls", "ins buffers", "other calls", "other buffers"
    );
    for (label, p) in [("create", create_profile), ("update", update_profile)] {
        println!(
            "{label:<12} {:>12} {:>14} {:>12} {:>14}",
            p.0, p.1, p.2, p.3
        );
    }

    explain(
        &mut conn,
        "single-row INSERT shape (one per link, unbatched, the pre-fix defect)",
        "INSERT INTO collection_links (collection_id, label, url, position) \
         VALUES (1, 'Explain Demo', 'https://example.org/explain-demo', 999)",
    );
    let batched_values = (0..3)
        .map(|i| {
            format!(
                "(1, 'Explain Batch {i}', 'https://example.org/explain-batch-{i}', {})",
                1000 + i
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    explain(
        &mut conn,
        "batched multi-row INSERT shape (the fix, one call for the whole link set)",
        &format!(
            "INSERT INTO collection_links (collection_id, label, url, position) VALUES {batched_values}"
        ),
    );
}

fn expected_for(links: &[(String, String)]) -> Vec<LinkRow> {
    links
        .iter()
        .enumerate()
        .map(|(i, (label, url))| LinkRow {
            label: label.clone(),
            url: url.clone(),
            position: i as i32,
        })
        .collect()
}
