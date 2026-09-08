//! Ledger findings/fix harness for the `dependent(..., on_delete = destroy)`
//! cascade's per-row loop (`autumn-macros/src/repository.rs`,
//! `__autumn_apply_dependent_on_conn`'s `Destroy` arm), driven through the
//! REAL production path: `LedgerDdPostRepository::delete_by_id`, the same
//! generated method any `DELETE /api/{resource}/{id}` handler calls.
//!
//! # The mechanism
//!
//! For a child model declared `dependent(ChildRepo, fk = "...", on_delete =
//! destroy)`, the generated cascade:
//!
//!   1. `SELECT id FROM child WHERE fk = $1 ... FOR UPDATE` (one statement).
//!   2. A restrict pre-scan pass over every returned id (a no-op loop when the
//!      child has no `restrict` grandchildren of its own).
//!   3. A mutating pass over every returned id: reload the row
//!      (`child.find(id).first()`), then delete it
//!      (`diesel::delete(child.find(id))`) — **two more statements per id**.
//!
//! For a "leaf" child — no `hooks`, not `versioned`, no `commit_hooks`,
//! nothing needing a `broadcasts` fragment, never soft-deleted, and no
//! `dependent(...)` grandchildren of its own (`LedgerDdComment` below is
//! exactly this: a plain blog-comment row) — step 3's reload is pure waste:
//! the plain-hard-delete arm of `#destroy_mutation` deletes by id alone and
//! never reads the reloaded record, and nothing else in that arm needs it
//! either. The whole cascade is 2N+1 statements to remove N rows that a
//! single `DELETE FROM child WHERE fk = $1` would remove in one.
//!
//! The fix routes this exact configuration through
//! `autumn_web::repository::dependent_delete_all` — the SAME runtime helper
//! the `on_delete = delete_all` action already calls, which already batches
//! its optional counter-cache decrement via `counter_cache_before_delete_many`
//! — guarded by a runtime check that the child has no *model-attribute*
//! `#[has_many(dependent = ...)]` grandchildren either (invisible to the
//! macro at the repository-attribute call site), so a model with runtime
//! grandchildren still gets the untouched per-row loop.
//!
//! # Fixture
//!
//! 500 posts (`ledger_dd_posts`) with a skewed comment fan-out
//! (`ledger_dd_comments`, real cardinality skew, not uniform):
//!   - 485 "ordinary" posts: 5-29 comments each (`5 + id % 25`).
//!   - 12 "trending" posts: 200-499 comments each (`200 + id % 300`).
//!   - 3 "viral" posts: 2,000 / 3,500 / 5,000 comments.
//!
//! ~23,000 comments total. `author` is 20% NULL (real NULL density);
//! `edited_at` is NULL for 70% of comments, else a timestamp (a comment that
//! was actually edited after posting) — real NULL density on the column an
//! equivalence check below also has to handle. A follow-up `UPDATE` before
//! `ANALYZE` gives the table real dead tuples.
//!
//! The measured operation is a single `delete_by_id` on the largest viral
//! post (5,000 comments) — a "delete this post" moderation action, the same
//! shape whether the post has 5 comments or 5,000: the per-row loop this
//! fixes runs once per `delete_by_id` call regardless, so a large fan-out
//! makes the existing N+1 tax visible, it doesn't create it.
//!
//! **Requires Docker.** Run manually with:
//!
//! ```text
//! cargo test -p autumn-web --features "test-support" \
//!   --test integration_tests -- --ignored \
//!   repository_dependent_destroy_leaf_batch_profile \
//!   --nocapture --test-threads=1
//! ```

#![cfg(feature = "db")]
#![allow(clippy::cast_precision_loss, clippy::too_many_lines)]

use autumn_web::current::with_actor;
use diesel::connection::SimpleConnection;
use diesel::sql_types::{BigInt, Text};
use diesel::{Connection, PgConnection, QueryableByName};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

diesel::table! {
    ledger_dd_posts (id) {
        id -> Int8,
        title -> Text,
    }
}

#[autumn_web::model(table = "ledger_dd_posts")]
pub struct LedgerDdPost {
    #[id]
    pub id: i64,
    pub title: String,
}

#[autumn_web::repository(
    LedgerDdPost,
    table = "ledger_dd_posts",
    dependent(PgLedgerDdCommentRepository, fk = "post_id", on_delete = destroy)
)]
pub trait LedgerDdPostRepository {}

diesel::table! {
    ledger_dd_comments (id) {
        id -> Int8,
        post_id -> Int8,
        body -> Text,
        author -> Nullable<Text>,
        edited_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "ledger_dd_comments")]
pub struct LedgerDdComment {
    #[id]
    pub id: i64,
    pub post_id: i64,
    pub body: String,
    #[default]
    pub author: Option<String>,
    #[default]
    pub edited_at: Option<chrono::NaiveDateTime>,
}

// A plain leaf: no hooks, not soft-delete, not versioned, no counter cache,
// no `dependent(...)` of its own — exactly the configuration the fast path
// requires.
#[autumn_web::repository(LedgerDdComment, table = "ledger_dd_comments")]
pub trait LedgerDdCommentRepository {}

const TOTAL_POSTS: i64 = 500;
const VIRAL_POST_ID: i64 = 500;
const VIRAL_POST_COMMENTS: i64 = 5_000;

const CREATE_TABLES_SQL: &str = "
    CREATE TABLE ledger_dd_posts (
        id BIGSERIAL PRIMARY KEY,
        title TEXT NOT NULL
    );
    CREATE TABLE ledger_dd_comments (
        id BIGSERIAL PRIMARY KEY,
        post_id BIGINT NOT NULL REFERENCES ledger_dd_posts(id),
        body TEXT NOT NULL,
        author TEXT,
        edited_at TIMESTAMP
    );
    CREATE INDEX ledger_dd_comments_post_id_idx ON ledger_dd_comments(post_id);
";

/// Skewed fan-out (485 ordinary / 12 trending / 3 viral posts, the last one
/// pinned to `VIRAL_POST_COMMENTS`) with real NULL density on `author`
/// (20%) and `edited_at` (70%), plus dead tuples from a follow-up `UPDATE`
/// before `ANALYZE` — the fixture shape the Ledger process requires.
fn seed_fixture(conn: &mut PgConnection) {
    conn.batch_execute(&format!(
        "INSERT INTO ledger_dd_posts (title) \
         SELECT 'post_' || gs FROM generate_series(1, {TOTAL_POSTS}) AS gs"
    ))
    .expect("seed ledger_dd_posts");

    // 485 ordinary posts: 5-29 comments each.
    conn.batch_execute(
        "INSERT INTO ledger_dd_comments (post_id, body, author, edited_at) \
         SELECT p.id, 'comment ' || c_gs || ' on post ' || p.id, \
                CASE WHEN c_gs % 5 = 0 THEN NULL ELSE 'user_' || (c_gs % 40) END, \
                CASE WHEN c_gs % 10 < 3 THEN TIMESTAMP '2024-01-01' + (c_gs || ' minutes')::interval ELSE NULL END \
         FROM ledger_dd_posts p \
         CROSS JOIN LATERAL generate_series(1, 5 + (p.id % 25)) AS c_gs \
         WHERE p.id <= 485",
    )
    .expect("seed ordinary comments");

    // 12 trending posts (486-497): 200-499 comments each.
    conn.batch_execute(
        "INSERT INTO ledger_dd_comments (post_id, body, author, edited_at) \
         SELECT p.id, 'comment ' || c_gs || ' on post ' || p.id, \
                CASE WHEN c_gs % 5 = 0 THEN NULL ELSE 'user_' || (c_gs % 40) END, \
                CASE WHEN c_gs % 10 < 3 THEN TIMESTAMP '2024-01-01' + (c_gs || ' minutes')::interval ELSE NULL END \
         FROM ledger_dd_posts p \
         CROSS JOIN LATERAL generate_series(1, 200 + (p.id % 300)) AS c_gs \
         WHERE p.id BETWEEN 486 AND 497",
    )
    .expect("seed trending comments");

    // 2 viral posts (498, 499): 2,000 / 3,500 comments.
    for (post_id, count) in [(498i64, 2_000i64), (499, 3_500)] {
        conn.batch_execute(&format!(
            "INSERT INTO ledger_dd_comments (post_id, body, author, edited_at) \
             SELECT {post_id}, 'comment ' || c_gs || ' on post ' || {post_id}, \
                    CASE WHEN c_gs % 5 = 0 THEN NULL ELSE 'user_' || (c_gs % 40) END, \
                    CASE WHEN c_gs % 10 < 3 THEN TIMESTAMP '2024-01-01' + (c_gs || ' minutes')::interval ELSE NULL END \
             FROM generate_series(1, {count}) AS c_gs"
        ))
        .unwrap_or_else(|e| panic!("seed viral comments for post {post_id}: {e}"));
    }

    // The measured viral post (500): exactly VIRAL_POST_COMMENTS comments.
    conn.batch_execute(&format!(
        "INSERT INTO ledger_dd_comments (post_id, body, author, edited_at) \
         SELECT {VIRAL_POST_ID}, 'comment ' || c_gs || ' on post ' || {VIRAL_POST_ID}, \
                CASE WHEN c_gs % 5 = 0 THEN NULL ELSE 'user_' || (c_gs % 40) END, \
                CASE WHEN c_gs % 10 < 3 THEN TIMESTAMP '2024-01-01' + (c_gs || ' minutes')::interval ELSE NULL END \
         FROM generate_series(1, {VIRAL_POST_COMMENTS}) AS c_gs"
    ))
    .expect("seed measured viral post comments");

    // Real dead tuples: touch a slice of rows post-insert.
    conn.batch_execute(
        "UPDATE ledger_dd_comments SET body = body || ' (edited)' WHERE id % 11 = 0",
    )
    .expect("create dead tuples");
    conn.batch_execute("ANALYZE ledger_dd_posts")
        .expect("analyze posts");
    conn.batch_execute("ANALYZE ledger_dd_comments")
        .expect("analyze comments");
}

#[derive(QueryableByName, Debug)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    n: i64,
}

fn reset_stats(conn: &mut PgConnection) {
    conn.batch_execute("SELECT pg_stat_statements_reset()")
        .expect("reset pg_stat_statements");
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

/// Prints every `ledger_dd_comments` statement from this run and returns
/// `(total_calls, total_buffers)` summed across every distinct statement
/// shape touching the comments table.
fn print_profile(conn: &mut PgConnection, label: &str) -> (i64, i64) {
    use diesel::RunQueryDsl;
    println!("\n=== pg_stat_statements: {label} ===");
    let rows = diesel::sql_query(
        "SELECT query, calls, (shared_blks_hit + shared_blks_read) AS buffers \
         FROM pg_stat_statements \
         WHERE query ILIKE '%ledger_dd_comments%' \
           AND query NOT ILIKE '%pg_stat_statements%' \
         ORDER BY calls DESC",
    )
    .load::<StatementRow>(conn)
    .expect("query pg_stat_statements");

    let (mut total_calls, mut total_buffers) = (0i64, 0i64);
    for row in &rows {
        let normalized = row.query.split_whitespace().collect::<Vec<_>>().join(" ");
        println!(
            "calls={:<6} buffers={:<8} {normalized}",
            row.calls, row.buffers
        );
        total_calls += row.calls;
        total_buffers += row.buffers;
    }
    println!("-- ledger_dd_comments total: calls={total_calls} buffers={total_buffers} --");
    (total_calls, total_buffers)
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
async fn repository_dependent_destroy_leaf_batch_profile() {
    use diesel::RunQueryDsl;

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
            "pg_stat_statements.max=5000",
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
    conn.batch_execute(CREATE_TABLES_SQL)
        .expect("create ledger_dd_posts / ledger_dd_comments");

    seed_fixture(&mut conn);

    let total_comments = diesel::sql_query("SELECT COUNT(*) AS n FROM ledger_dd_comments")
        .get_result::<CountRow>(&mut conn)
        .expect("count all comments")
        .n;
    let viral_comments_before = diesel::sql_query(format!(
        "SELECT COUNT(*) AS n FROM ledger_dd_comments WHERE post_id = {VIRAL_POST_ID}"
    ))
    .get_result::<CountRow>(&mut conn)
    .expect("count viral post comments")
    .n;
    println!(
        "\n-- fixture: {total_comments} total comments across {TOTAL_POSTS} posts; \
         post {VIRAL_POST_ID} has {viral_comments_before} comments (expected {VIRAL_POST_COMMENTS}) --"
    );
    assert_eq!(
        viral_comments_before, VIRAL_POST_COMMENTS,
        "measured post's comment count must match the fixture's documented shape"
    );
    assert!(
        total_comments > VIRAL_POST_COMMENTS * 2,
        "the long tail (ordinary + trending + other viral posts) must dominate \
         the measured post, or this fixture isn't production-shaped"
    );

    // A representative OTHER row, untouched by the delete, to prove the
    // cascade doesn't touch rows outside the target post (result-equivalence
    // edge case: a sibling post's comments, including one with a NULL
    // `author`/`edited_at`, must survive byte-for-byte).
    let sibling_checksum_before =
        diesel::sql_query("SELECT COUNT(*) AS n FROM ledger_dd_comments WHERE post_id = 1")
            .get_result::<CountRow>(&mut conn)
            .expect("count sibling comments before")
            .n;

    let config = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(config).build().expect("pool");
    let post_repo = PgLedgerDdPostRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    };

    reset_stats(&mut conn);
    with_actor("system", async {
        post_repo
            .delete_by_id(VIRAL_POST_ID)
            .await
            .expect("delete_by_id must cascade-destroy the post's comments")
    })
    .await;

    let (total_calls, total_buffers) = print_profile(&mut conn, "delete_by_id(viral post)");
    println!(
        "\n-- statement-count claim: {VIRAL_POST_COMMENTS} comments cascaded, \
         ledger_dd_comments statements: calls={total_calls} buffers={total_buffers} --"
    );

    // ── Correctness: cascade removed exactly the target post's comments ──
    let remaining_for_post = diesel::sql_query(format!(
        "SELECT COUNT(*) AS n FROM ledger_dd_comments WHERE post_id = {VIRAL_POST_ID}"
    ))
    .get_result::<CountRow>(&mut conn)
    .expect("count remaining viral comments")
    .n;
    assert_eq!(
        remaining_for_post, 0,
        "every comment belonging to the deleted post must be gone"
    );

    let post_gone = diesel::sql_query(format!(
        "SELECT COUNT(*) AS n FROM ledger_dd_posts WHERE id = {VIRAL_POST_ID}"
    ))
    .get_result::<CountRow>(&mut conn)
    .expect("count post row")
    .n;
    assert_eq!(post_gone, 0, "the post itself must be deleted");

    let total_remaining = diesel::sql_query("SELECT COUNT(*) AS n FROM ledger_dd_comments")
        .get_result::<CountRow>(&mut conn)
        .expect("count all remaining comments")
        .n;
    assert_eq!(
        total_remaining,
        total_comments - VIRAL_POST_COMMENTS,
        "exactly the target post's comments must be removed, no more, no less"
    );

    let sibling_checksum_after =
        diesel::sql_query("SELECT COUNT(*) AS n FROM ledger_dd_comments WHERE post_id = 1")
            .get_result::<CountRow>(&mut conn)
            .expect("count sibling comments after")
            .n;
    assert_eq!(
        sibling_checksum_before, sibling_checksum_after,
        "a sibling post's comments (including NULL author/edited_at rows) must survive untouched"
    );

    // ── The N+1 floor claim, pinned as an assertion ──
    //
    // A cascade over VIRAL_POST_COMMENTS rows must not scale statement count
    // (or buffers) with the number of rows destroyed. Pre-fix this was
    // 2*VIRAL_POST_COMMENTS + 1 (one id-select, one reload, one delete per
    // row); post-fix it is a small constant (one batched
    // `DELETE ... WHERE post_id = $1`).
    assert!(
        total_calls < 10,
        "the destroy cascade over a leaf child must issue a small constant \
         number of statements regardless of fan-out, not one per row \
         destroyed; got {total_calls} calls for {VIRAL_POST_COMMENTS} rows"
    );
    // Buffers still scale with rows (the batched DELETE reads/writes each row
    // once), but must land near a single scan's worth, not the pre-fix
    // shape's ~9 buffers/row (one FOR-UPDATE reload plus one point-delete per
    // row, on top of the initial id scan) -- measured 45,583 buffers
    // pre-fix, 5,126 post-fix, for the same 5,000-row cascade. `* 3` leaves
    // headroom above the measured ~1.03 buffers/row without letting a
    // regression back toward the old per-row shape pass silently.
    assert!(
        total_buffers < VIRAL_POST_COMMENTS * 3,
        "buffers touched must not scale like the old per-row reload+delete \
         shape (~9 buffers/row); got {total_buffers} buffers for {VIRAL_POST_COMMENTS} rows"
    );

    conn.transaction::<(), diesel::result::Error, _>(|conn| {
        explain(
            conn,
            "batched DELETE by post_id = $1 (the actual post-fix statement \
             shape), against post 499's 3,500 comments, rolled back — diagnostic only",
            "DELETE FROM ledger_dd_comments WHERE post_id = 499 RETURNING id",
        );
        Err(diesel::result::Error::RollbackTransaction)
    })
    .ok();

    conn.transaction::<(), diesel::result::Error, _>(|conn| {
        explain(
            conn,
            "point SELECT+DELETE by id (the pre-fix loop's per-row statement \
             shape), rolled back — diagnostic only",
            "SELECT * FROM ledger_dd_comments WHERE id = \
             (SELECT id FROM ledger_dd_comments WHERE post_id = 499 LIMIT 1)",
        );
        Err(diesel::result::Error::RollbackTransaction)
    })
    .ok();
}
