//! Ledger profiling harness for `content::recount_terms`
//! (`examples/cms/src/content.rs`, ~line 1217-1268), driven through the real
//! `POST /admin/content/{post_type}/{id}` route — the ordinary editor
//! "Update" button, not an admin bulk tool. This is a distinct N+1 from the
//! already-fixed `content::import_terms` taxonomy lookups
//! (`import_terms_batch_profile.rs`) and the `content::lock_terms` fan-out
//! (`set_post_terms_lock_batch_profile.rs`, at the time of this harness still
//! an open PR, not yet merged) — do not confuse the three. This one lives
//! entirely inside `recount_terms`, one call further down the same
//! `set_post_terms` transaction.
//!
//! `recount_terms(conn, term_ids)` sorts and dedups its input, then loops the
//! ids **one at a time** and calls `recount_term(conn, term_id)` per id.
//! `recount_term` itself issues three round trips per call:
//!
//! 1. `SELECT "terms"."id" FROM "terms" WHERE "terms"."id" = $1 FOR UPDATE`
//!    — the identical shape `lock_terms` (a separate function, one call
//!    earlier in `set_post_terms`) also issues per id, so `pg_stat_statements`
//!    folds the two loops' calls into one normalized row. This harness's
//!    `single_lock` counter is therefore `lock_terms`'s N contributions PLUS
//!    `recount_term`'s N contributions, exactly as
//!    `set_post_terms_lock_batch_profile.rs`'s own `single_lock_calls`
//!    documents — the fix in *this* PR does not touch `lock_terms`, so that
//!    half of the shared shape is unchanged by it; the isolated per-request
//!    delta this harness attributes to `recount_terms` is `single_lock`'s own
//!    N-sized drop.
//! 2. `SELECT COUNT(*) FROM "post_terms" INNER JOIN "posts" ON ... WHERE
//!    "post_terms"."term_id" = $1 AND "posts"."status" = $2 AND
//!    "posts"."post_type" = ANY($3)` — a single-term scalar count, once per
//!    id.
//! 3. `UPDATE "terms" SET "post_count" = $1 WHERE "terms"."id" = $2` — a
//!    single-row write, once per id.
//!
//! N ids therefore cost 3N round trips through this function alone (on top of
//! whatever `lock_terms` already cost), when the identity space `recount_terms`
//! needs to resolve is bounded by N distinct term rows, not by looping N
//! times. This harness's fix replaces the loop with: one batched
//! `"terms"."id" = ANY($1) ... ORDER BY "terms"."id" ASC FOR UPDATE` lock
//! (ascending order preserved — see the `EXPLAIN` at the bottom), one call to
//! the already-batched `term_post_counts` helper (a `GROUP BY term_id`
//! query, pre-existing but previously used only by read-path screens, never
//! by the write path that maintains the stored counter), and one bulk
//! `UPDATE terms SET post_count = data.count FROM (SELECT * FROM
//! UNNEST($1::bigint[], $2::bigint[]) AS t(id, count)) AS data WHERE
//! terms.id = data.id` in place of N single-row updates.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p cms --test recount_terms_batch_profile \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! This example crate has no consolidated Docker sweep (see CLAUDE.md) —
//! `examples/cms`'s own Docker-gated suites are named explicitly in
//! `ci.yml`'s "Run Docker-dependent tests" step. Like its siblings
//! (`import_terms_batch_profile`, `permalink_search_ancestry_batch_profile`),
//! this is a "run manually" profiling harness rather than a suite CI gates
//! on, so this file adds no `ci.yml` line.
//!
//! ## Fixture
//!
//! 5,000 pre-existing `terms` rows (2,500 `category`, 2,500 `post_tag`,
//! slugs prefixed `legacy-`), matching the sibling harnesses' background
//! scale, so the `terms` PK index this call locks against has a realistic
//! size. On top of that, 2,000 unrelated background `post`-type posts
//! (publish status) each filed under one or two of the legacy `category`
//! terms via `post_terms`, giving `recount_term`'s (and `term_post_counts`'s)
//! join+count a realistic ~3,000-row `post_terms` table to scan rather than
//! a table containing only the test post's own rows. Real dead tuples from a
//! follow-up `UPDATE` before `ANALYZE`, no `VACUUM`.
//!
//! Three tiers of one real CMS post moving from an existing `category`
//! selection to a new, **partially overlapping** one (not fully disjoint, not
//! identical), submitted via repeated `taxonomies[category]=<id>` fields on
//! `POST /admin/content/post/{id}` — the actual editor "Update" request:
//!
//! | tier   | previous | wanted | overlap | affected (union) |
//! |--------|---------:|-------:|--------:|------------------:|
//! | small  |        3 |      5 |       1 |                 7 |
//! | medium |       10 |     20 |       3 |                27 |
//! | large  |       25 |     50 |      10 |                65 |
//!
//! `wanted` never exceeds `MAX_TERMS_PER_SAVE` (50, `posts.rs:725`) — the
//! large tier sits exactly at the cap. Each tier gets its own pool of freshly
//! created `category` terms (not the legacy background ones), so tiers never
//! share ids and each tier's own `pg_stat_statements` reset window is clean.

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

/// Mirrors `import_terms_batch_profile.rs`'s helper of the same name (and
/// `tests/integration_test.rs`'s `migration_statements`).
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
/// (`Role::Administrator`, every capability including `EditPosts`) — the
/// election in `content::register_user` runs on `users::table.count() == 0`,
/// so this must run **before** any other user row is inserted, including the
/// raw-SQL background author seeded below.
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

/// Create a post through the real admin editor, returning its id. Mirrors
/// `tests/integration_test.rs`'s `create_post`, plus a repeated
/// `taxonomies[category]=<id>` field per selected category.
async fn create_post(client: &TestClient, cookie: &str, title: &str, category_ids: &[i64]) -> i64 {
    let mut fields: Vec<(String, String)> = vec![
        ("title".to_owned(), title.to_owned()),
        ("slug".to_owned(), String::new()),
        ("excerpt".to_owned(), String::new()),
        ("body".to_owned(), "Fixture post body.".to_owned()),
        ("status".to_owned(), "publish".to_owned()),
        ("password".to_owned(), String::new()),
        ("taxonomy_names[post_tag]".to_owned(), String::new()),
        ("comment_status".to_owned(), "open".to_owned()),
    ];
    for id in category_ids {
        fields.push(("taxonomies[category]".to_owned(), id.to_string()));
    }
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let resp = client
        .post("/admin/content/post")
        .header("cookie", cookie)
        .form(&form(&pairs))
        .send()
        .await;
    assert_eq!(resp.status, 303, "create should redirect: {}", resp.text());
    resp.header("location")
        .expect("redirect to the editor")
        .rsplit('/')
        .next()
        .expect("id is the last path segment")
        .parse()
        .expect("id is numeric")
}

/// The post's current `lock_version`, read directly from the database — the
/// update handler requires the form to carry it (stale-edit detection), same
/// as `tests/integration_test.rs`'s `lock_version_of`.
#[derive(QueryableByName)]
struct LockVersionRow {
    #[diesel(sql_type = diesel::sql_types::Integer)]
    lock_version: i32,
}
fn lock_version_of(conn: &mut PgConnection, id: i64) -> i32 {
    use diesel::RunQueryDsl;
    diesel::sql_query("SELECT lock_version FROM posts WHERE id = $1")
        .bind::<BigInt, _>(id)
        .get_result::<LockVersionRow>(conn)
        .expect("post exists")
        .lock_version
}

/// Update the post through the real admin editor with a new `category`
/// selection — the request this harness profiles.
async fn update_post_terms(
    client: &TestClient,
    cookie: &str,
    id: i64,
    title: &str,
    lock_version: i32,
    category_ids: &[i64],
) -> TestResponse {
    let mut fields: Vec<(String, String)> = vec![
        ("title".to_owned(), title.to_owned()),
        ("slug".to_owned(), String::new()),
        ("excerpt".to_owned(), String::new()),
        ("body".to_owned(), "Fixture post body, updated.".to_owned()),
        ("status".to_owned(), "publish".to_owned()),
        ("password".to_owned(), String::new()),
        ("taxonomy_names[post_tag]".to_owned(), String::new()),
        ("comment_status".to_owned(), "open".to_owned()),
        ("lock_version".to_owned(), lock_version.to_string()),
    ];
    for id in category_ids {
        fields.push(("taxonomies[category]".to_owned(), id.to_string()));
    }
    let pairs: Vec<(&str, &str)> = fields
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    client
        .post(&format!("/admin/content/post/{id}"))
        .header("cookie", cookie)
        .form(&form(&pairs))
        .send()
        .await
}

const NUM_LEGACY_CATEGORIES: i64 = 2_500;
const NUM_LEGACY_TAGS: i64 = 2_500;
const NUM_BACKGROUND_POSTS: i64 = 2_000;

/// The site's taxonomy and post/term-filing background: 5,000 unrelated
/// pre-existing terms (matching the sibling harnesses' scale) plus 2,000
/// unrelated published posts, each filed under one or two of the legacy
/// `category` terms via `post_terms` — so `recount_term`'s (and
/// `term_post_counts`'s) join+count runs against a realistically populated
/// table, not an empty one.
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

    // A background author distinct from the registered owner — inserted
    // AFTER `register()` runs, so it never wins the site-owner election.
    conn.batch_execute(
        "INSERT INTO users (username, email, password_hash, display_name, role) \
         VALUES ('bg-author', 'bg-author@example.com', 'x', 'Background Author', 'author')",
    )
    .expect("seed background author");
    let bg_author_id: i64 = {
        #[derive(QueryableByName)]
        struct IdRow {
            #[diesel(sql_type = BigInt)]
            id: i64,
        }
        use diesel::RunQueryDsl;
        diesel::sql_query("SELECT id FROM users WHERE username = 'bg-author'")
            .get_result::<IdRow>(conn)
            .expect("background author id")
            .id
    };

    conn.batch_execute(&format!(
        "INSERT INTO posts \
         (post_type, title, slug, excerpt, body, status, author_id, \
          published_at, created_at, updated_at) \
         SELECT \
           'post', 'Background Post ' || i, 'bg-post-' || i, '', \
           'Background filler body ' || i || '.', 'publish', {bg_author_id}, \
           TIMESTAMP '2024-01-01 00:00:00', TIMESTAMP '2024-01-01 00:00:00', \
           TIMESTAMP '2024-01-01 00:00:00' \
         FROM generate_series(1, {NUM_BACKGROUND_POSTS}) AS i"
    ))
    .expect("seed background posts");

    // Each background post filed under one or two legacy categories — a
    // realistic pre-existing `post_terms` cardinality unrelated to the tier
    // posts this harness actually profiles.
    conn.batch_execute(&format!(
        "INSERT INTO post_terms (post_id, term_id) \
         SELECT p.id, t.id FROM posts p \
         JOIN terms t ON t.taxonomy = 'category' \
           AND t.slug = 'legacy-cat-' || (1 + (p.id % {NUM_LEGACY_CATEGORIES})) \
         WHERE p.slug LIKE 'bg-post-%'"
    ))
    .expect("seed background post_terms (first category)");
    conn.batch_execute(&format!(
        "INSERT INTO post_terms (post_id, term_id) \
         SELECT p.id, t.id FROM posts p \
         JOIN terms t ON t.taxonomy = 'category' \
           AND t.slug = 'legacy-cat-' || (1 + ((p.id * 7) % {NUM_LEGACY_CATEGORIES})) \
         WHERE p.slug LIKE 'bg-post-%' AND p.id % 2 = 0 \
         ON CONFLICT (post_id, term_id) DO NOTHING"
    ))
    .expect("seed background post_terms (second category, half the posts)");

    // Real dead tuples: touch a slice of rows post-insert, same technique
    // every other Ledger fixture in this repo uses, no intervening VACUUM.
    conn.batch_execute("UPDATE terms SET description = 'reconciled' WHERE id % 7 = 0")
        .expect("create dead tuples in terms");
    conn.batch_execute(
        "UPDATE posts SET excerpt = 'reconciled' WHERE id % 7 = 0 AND slug LIKE 'bg-post-%'",
    )
    .expect("create dead tuples in posts");
    conn.batch_execute("ANALYZE terms").expect("analyze terms");
    conn.batch_execute("ANALYZE posts").expect("analyze posts");
    conn.batch_execute("ANALYZE post_terms")
        .expect("analyze post_terms");
}

/// One tier's shape: `previous_count` currently-filed categories moving to
/// `wanted_count` newly-submitted ones, sharing `overlap_count` ids at the
/// boundary — so `affected` (the union, what `recount_terms` receives) is a
/// realistic partial overlap, not a coincidence of disjoint or identical
/// sets.
struct Tier {
    name: &'static str,
    previous_count: usize,
    wanted_count: usize,
    overlap_count: usize,
}

const TIERS: [Tier; 3] = [
    Tier {
        name: "small",
        previous_count: 3,
        wanted_count: 5,
        overlap_count: 1,
    },
    Tier {
        name: "medium",
        previous_count: 10,
        wanted_count: 20,
        overlap_count: 3,
    },
    Tier {
        name: "large",
        previous_count: 25,
        wanted_count: 50,
        overlap_count: 10,
    },
];

impl Tier {
    /// Total distinct category terms this tier needs: `previous ∪ wanted`,
    /// arranged as one contiguous pool so the overlap sits at the boundary.
    fn pool_size(&self) -> usize {
        self.previous_count + self.wanted_count - self.overlap_count
    }

    /// Index range into the tier's pool for the "previous" (currently filed)
    /// set: `[0, previous_count)`.
    fn previous_range(&self) -> std::ops::Range<usize> {
        0..self.previous_count
    }

    /// Index range into the tier's pool for the "wanted" (newly submitted)
    /// set: starts `overlap_count` before `previous_count` ends, so the two
    /// ranges share exactly `overlap_count` indices.
    fn wanted_range(&self) -> std::ops::Range<usize> {
        let start = self.previous_count - self.overlap_count;
        start..(start + self.wanted_count)
    }
}

/// Insert `pool_size` fresh `category` terms for one tier, returning their
/// ids in insertion (ascending) order.
fn seed_tier_pool(conn: &mut PgConnection, tier: &Tier) -> Vec<i64> {
    #[derive(QueryableByName)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
    use diesel::RunQueryDsl;
    diesel::sql_query(format!(
        "INSERT INTO terms (taxonomy, name, slug, description, parent_id, created_at) \
         SELECT 'category', 'Tier {name} Term ' || i, 'tier-{name}-' || i, '', NULL, \
                TIMESTAMP '2026-01-01 00:00:00' \
         FROM generate_series(1, {size}) AS i \
         RETURNING id",
        name = tier.name,
        size = tier.pool_size()
    ))
    .load::<IdRow>(conn)
    .expect("seed tier category pool")
    .into_iter()
    .map(|row| row.id)
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

/// A profile of every statement `pg_stat_statements` recorded since the last
/// reset — i.e. exactly the one `POST /admin/content/post/{id}` request under
/// test, since nothing else touches this connection pool between reset and
/// request.
struct Profile {
    /// Every statement's calls, summed — the whole request's statement count.
    total_calls: i64,
    total_buffers: i64,
    /// `"terms"."id" = $1 ... FOR UPDATE` (no `ANY(`). Pre-fix this is
    /// `lock_terms`'s N calls PLUS `recount_term`'s N re-locking calls
    /// (identical generated SQL, folded by `pg_stat_statements` into one
    /// row — see the module doc comment). Post-fix, only `lock_terms`
    /// (unfixed by this PR) contributes: still N, not 2N.
    single_lock_calls: i64,
    /// `"terms"."id" = ANY($1) ... FOR UPDATE` — `recount_terms`'s own
    /// batched lock. Zero pre-fix, exactly 1 per request post-fix.
    any_lock_calls: i64,
    /// The single-term scalar `COUNT(*)` `recount_term` issues per id. N
    /// pre-fix, 0 post-fix.
    single_count_calls: i64,
    /// `term_post_counts`'s grouped, batched count query. 0 pre-fix
    /// (nothing on this codepath called it), 1 post-fix.
    grouped_count_calls: i64,
    /// `UPDATE "terms" SET "post_count" = $1 WHERE "terms"."id" = $2` — one
    /// per id. N pre-fix, 0 post-fix.
    single_update_calls: i64,
    /// The bulk `UPDATE terms ... FROM (SELECT * FROM UNNEST(...))` this fix
    /// issues. 0 pre-fix, 1 post-fix.
    bulk_update_calls: i64,
}

fn print_profile(conn: &mut PgConnection, label: &str) -> Profile {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let mut p = Profile {
        total_calls: 0,
        total_buffers: 0,
        single_lock_calls: 0,
        any_lock_calls: 0,
        single_count_calls: 0,
        grouped_count_calls: 0,
        single_update_calls: 0,
        bulk_update_calls: 0,
    };
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        p.total_calls += row.calls;
        p.total_buffers += row.buffers;

        let is_terms_lock = normalized.contains("\"terms\"") && normalized.contains("FOR UPDATE");
        let is_count = normalized.contains("COUNT(") && normalized.contains("post_terms");
        let is_single_update = normalized.contains("UPDATE \"terms\" SET \"post_count\"");
        let is_bulk_update = normalized.to_uppercase().contains("UNNEST");

        if is_terms_lock && normalized.contains("= ANY(") {
            p.any_lock_calls += row.calls;
        } else if is_terms_lock {
            p.single_lock_calls += row.calls;
        } else if is_count && normalized.contains("GROUP BY") {
            p.grouped_count_calls += row.calls;
        } else if is_count {
            p.single_count_calls += row.calls;
        } else if is_bulk_update {
            p.bulk_update_calls += row.calls;
        } else if is_single_update {
            p.single_update_calls += row.calls;
        }
    }
    println!(
        "-- total: calls={} buffers={} -- single-row terms lock (lock_terms + recount_term \
         pre-fix, lock_terms alone post-fix): {} -- batched ANY() terms lock (this fix): {} -- \
         single-row count (recount_term pre-fix): {} -- grouped batched count (this fix): {} -- \
         single-row update (recount_term pre-fix): {} -- bulk UNNEST update (this fix): {} --",
        p.total_calls,
        p.total_buffers,
        p.single_lock_calls,
        p.any_lock_calls,
        p.single_count_calls,
        p.grouped_count_calls,
        p.single_update_calls,
        p.bulk_update_calls
    );
    p
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
    for line in &lines {
        println!("{}", line.line);
    }
}

#[derive(QueryableByName, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TermIdRow {
    #[diesel(sql_type = BigInt)]
    term_id: i64,
}

/// The set of term ids actually linking `post_id` in `post_terms`, sorted.
fn post_term_ids(conn: &mut PgConnection, post_id: i64) -> Vec<i64> {
    use diesel::RunQueryDsl;
    let mut rows = diesel::sql_query("SELECT term_id FROM post_terms WHERE post_id = $1")
        .bind::<BigInt, _>(post_id)
        .load::<TermIdRow>(conn)
        .expect("post_terms for post");
    rows.sort();
    rows.into_iter().map(|r| r.term_id).collect()
}

#[derive(QueryableByName, Debug)]
struct PostCountRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = BigInt)]
    post_count: i64,
    #[diesel(sql_type = BigInt)]
    live_count: i64,
}

/// For each of `term_ids`, the stored `terms.post_count` next to a
/// ground-truth recomputation (`post_terms` joined to `publish` posts) — the
/// same predicate `recount_term`/`term_post_counts` use, queried
/// independently here so the equivalence check does not just restate the
/// code under test.
fn stored_vs_live_post_counts(conn: &mut PgConnection, term_ids: &[i64]) -> Vec<PostCountRow> {
    use diesel::RunQueryDsl;
    let list = term_ids
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    diesel::sql_query(format!(
        "SELECT t.id, t.post_count, \
                (SELECT count(*) FROM post_terms pt JOIN posts p ON p.id = pt.post_id \
                  WHERE pt.term_id = t.id AND p.status = 'publish' \
                    AND p.post_type IN ('post', 'page')) AS live_count \
         FROM terms t WHERE t.id IN ({list}) ORDER BY t.id"
    ))
    .load::<PostCountRow>(conn)
    .expect("stored vs live post_count")
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
#[allow(clippy::too_many_lines)]
async fn recount_terms_batch_profile() {
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

    // Site owner registered FIRST, before any background user row exists —
    // see `register`'s doc comment.
    let cookie = register(&client, "owner").await;

    seed_background(&mut conn);

    let mut tier_results = Vec::new();
    let mut large_tier_affected: Vec<i64> = Vec::new();
    for tier in &TIERS {
        let pool_ids = seed_tier_pool(&mut conn, tier);
        let previous_ids = &pool_ids[tier.previous_range()];
        let wanted_ids = &pool_ids[tier.wanted_range()];
        let mut affected: Vec<i64> = previous_ids
            .iter()
            .chain(wanted_ids.iter())
            .copied()
            .collect();
        affected.sort_unstable();
        affected.dedup();
        assert_eq!(
            affected.len(),
            tier.pool_size(),
            "tier {}: previous/wanted must union to exactly the tier's pool \
             (partial overlap, not disjoint or identical)",
            tier.name
        );

        let title = format!("Tier {} Post", tier.name);
        let id = create_post(&client, &cookie, &title, previous_ids).await;
        assert_eq!(
            post_term_ids(&mut conn, id),
            {
                let mut v = previous_ids.to_vec();
                v.sort_unstable();
                v
            },
            "tier {}: the post must be filed under exactly its initial category set",
            tier.name
        );

        // === The profiled request: the editor's "Update" save, which calls
        // set_post_terms -> lock_terms, then set_post_terms -> recount_terms
        // (the function this harness targets) ===
        let lock_version = lock_version_of(&mut conn, id);
        reset_stats(&mut conn);
        let resp = update_post_terms(&client, &cookie, id, &title, lock_version, wanted_ids).await;
        assert_eq!(
            resp.status,
            303,
            "tier {}: update should redirect; body was: {}",
            tier.name,
            resp.text()
        );
        let profile = print_profile(
            &mut conn,
            &format!(
                "tier {} (previous={}, wanted={}, overlap={}, affected={})",
                tier.name,
                tier.previous_count,
                tier.wanted_count,
                tier.overlap_count,
                affected.len()
            ),
        );

        // ── Equivalence (a): the exact same set of term rows ends up
        // post_terms-linked — the wanted set, nothing more, nothing less.
        let mut expected_wanted = wanted_ids.to_vec();
        expected_wanted.sort_unstable();
        expected_wanted.dedup();
        assert_eq!(
            post_term_ids(&mut conn, id),
            expected_wanted,
            "tier {}: after the update, post_terms must link exactly the wanted set",
            tier.name
        );

        // ── Equivalence (b): the stored terms.post_count matches ground
        // truth for every affected term. This is the correctness property
        // `recount_terms` exists to guarantee, and the fix must preserve it
        // exactly — not "close enough".
        for row in stored_vs_live_post_counts(&mut conn, &affected) {
            assert_eq!(
                row.post_count, row.live_count,
                "tier {}: term {} has stored post_count={} but live count={}",
                tier.name, row.id, row.post_count, row.live_count
            );
        }

        if tier.name == "large" {
            large_tier_affected.clone_from(&affected);
        }
        tier_results.push((tier.name, affected.len(), profile));
    }

    println!("\n=== statement-count scaling across tiers ===");
    println!(
        "{:<8} {:>10} {:>12} {:>14} {:>13} {:>10} {:>14} {:>15} {:>15} {:>13}",
        "tier",
        "affected",
        "total calls",
        "total buffers",
        "single-lock",
        "any-lock",
        "single-count",
        "grouped-count",
        "single-update",
        "bulk-update"
    );
    for (name, affected, profile) in &tier_results {
        println!(
            "{name:<8} {affected:>10} {:>12} {:>14} {:>13} {:>10} {:>14} {:>15} {:>15} {:>13}",
            profile.total_calls,
            profile.total_buffers,
            profile.single_lock_calls,
            profile.any_lock_calls,
            profile.single_count_calls,
            profile.grouped_count_calls,
            profile.single_update_calls,
            profile.bulk_update_calls
        );
    }

    // ── Equivalence (c): the batched lock's own plan actually locks in
    // ascending id order. Uses the large tier's own 65 real ids, given to the
    // planner in DESCENDING order (the opposite of what `.order(terms::id.asc())`
    // asks for) — so if the plan did not truly guarantee ascending output, this
    // would be the case most likely to expose it.
    let mut descending = large_tier_affected.clone();
    descending.sort_unstable_by(|a, b| b.cmp(a));
    let any_list = descending
        .iter()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    explain(
        &mut conn,
        "single-row lock shape (recount_term pre-fix, one call per id, unbatched)",
        "SELECT \"terms\".\"id\" FROM \"terms\" WHERE \"terms\".\"id\" = 1 FOR UPDATE",
    );
    explain(
        &mut conn,
        "batched ANY() lock shape (the fix: sort ascending, then lock — one call for the \
         whole 65-id affected set, given to the planner pre-sorted DESCENDING to stress the \
         ordering guarantee)",
        &format!(
            "SELECT \"terms\".\"id\" FROM \"terms\" WHERE \"terms\".\"id\" = ANY(ARRAY[{any_list}]) \
             ORDER BY \"terms\".\"id\" ASC FOR UPDATE"
        ),
    );
    explain(
        &mut conn,
        "single-term scalar count shape (recount_term pre-fix, one call per id)",
        &format!(
            "SELECT COUNT(*) FROM post_terms pt JOIN posts p ON p.id = pt.post_id \
             WHERE pt.term_id = {} AND p.status = 'publish' AND p.post_type IN ('post', 'page')",
            large_tier_affected[0]
        ),
    );
    explain(
        &mut conn,
        "grouped batched count shape (the fix: term_post_counts, one call for the whole set)",
        &format!(
            "SELECT pt.term_id, COUNT(*) FROM post_terms pt JOIN posts p ON p.id = pt.post_id \
             WHERE pt.term_id = ANY(ARRAY[{any_list}]) AND p.status = 'publish' \
               AND p.post_type IN ('post', 'page') GROUP BY pt.term_id"
        ),
    );
}
