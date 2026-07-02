//! Integration tests: many-to-many (`through =`) associations + preload +
//! mutation helpers (#1324).
//!
//! Covers the issue's test-shaped acceptance criteria against a real
//! Postgres:
//!
//! 1. `m2m_preload_happy_path` -- `Post::preload().tags()` loads a post's
//!    tags through the `post_tags` join table and reads back through the
//!    typed `tags()` accessor.
//! 2. `m2m_preload_empty_is_empty_slice` -- a post with no tags yields an
//!    empty slice, not an error.
//! 3. `m2m_not_preloaded_is_typed_error` -- an un-preloaded `tags()` access
//!    returns `NotLoaded`, never SQL. (No Docker needed.)
//! 4. `m2m_nested_through_path` -- `tags_with(Tag::preload().posts())`, the
//!    mirror-image `through =` association on `Tag`, loads.
//! 5. `m2m_preload_is_batched_no_n_plus_one` -- the number of SQL statements
//!    a `tags()` preload issues does not grow with the number of tags on a
//!    post: a post with 2 tags and a post with 40 tags issue the same
//!    (small, fixed) number of statements.
//! 6. `add_tag_is_idempotent` -- a duplicate `add_tag` is a no-op (`ON
//!    CONFLICT DO NOTHING`), not a unique-constraint error.
//! 7. `remove_tag_unlinks` -- `remove_tag` unlinks a pair; removing an
//!    unlinked pair is also a no-op.
//! 8. `set_tags_replaces_all` -- `set_tags` replaces a post's full tag set
//!    in one transaction.
//! 9. `set_tags_dedupes_input` -- duplicate ids in `set_tags`' input collapse
//!    to one link each.
//!
//! The Docker-backed tests are `#[ignore]` (like the other PG integration
//! tests) and run via testcontainers by default. When Docker is unavailable
//! (e.g. this sandboxed environment), point them at an already-running
//! Postgres instead:
//!
//! ```text
//! AUTUMN_TEST_PG_URL=postgres://autumn:autumn@127.0.0.1:5432/reddit \
//!   cargo test -p reddit-clone --test m2m_pg_integration -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` matters only for the external-URL path: each test
//! resets the reddit-clone tables on the shared external database before
//! seeding, so parallel test functions would race on the same tables. A
//! fresh testcontainers Postgres has no such constraint (each test gets its
//! own throwaway container) and runs fine with the default parallel runner.

use autumn_web::preload::{Preloadable, Preloaded};
use diesel::prelude::*;
use diesel::sql_types::BigInt;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};

use reddit_clone::models::{Post, PostAssociations, PostTagsMutations, Tag, TagAssociations};
use reddit_clone::repositories::PgPostRepository;
use reddit_clone::schema::posts;
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const CREATE_SCHEMA: &str = include_str!("../migrations/20260419000000_create_reddit/up.sql");
// `User::as_select()` (used by other preload tests' `author` preloads)
// includes the `avatar` column added by this later migration; the shared
// schema-setup helper below applies it for parity, even though these tests
// don't preload `author`.
const ADD_AVATAR: &str = include_str!("../migrations/20260427000000_add_user_avatar/up.sql");
const CREATE_TAGS: &str = include_str!("../migrations/20260702000001_create_tags/up.sql");

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

/// Keeps whichever backing Postgres alive for the duration of the test: a
/// throwaway testcontainers container, or nothing (an already-running
/// external instance pointed to via `AUTUMN_TEST_PG_URL`).
enum PgHandle {
    Container(#[allow(dead_code)] ContainerAsync<Postgres>),
    External,
}

async fn start_postgres() -> (PgHandle, Pool<AsyncPgConnection>) {
    if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
        let pool = Pool::builder(manager)
            .max_size(4)
            .build()
            .expect("build pool");
        return (PgHandle::External, pool);
    }
    let container = Postgres::default()
        .start()
        .await
        .expect("start Postgres container");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("Postgres port");
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager)
        .max_size(4)
        .build()
        .expect("build pool");
    (PgHandle::Container(container), pool)
}

async fn setup_schema(conn: &mut AsyncPgConnection) {
    // An external Postgres is a persistent instance shared across runs (and,
    // within one run, across this file's tests when `--test-threads=1` is
    // honored) -- reset it to a clean slate first. A fresh testcontainers
    // Postgres never has these tables, so this is a no-op there.
    if std::env::var("AUTUMN_TEST_PG_URL").is_ok() {
        conn.batch_execute(
            "DROP TABLE IF EXISTS post_tags, tags, comments, votes, \
             live_feed_events, posts, subreddits, users CASCADE;",
        )
        .await
        .expect("reset reddit-clone tables");
    }
    // The migration files contain many semicolon-separated statements;
    // Postgres rejects multiple commands in a prepared statement, so use the
    // simple (unprepared) batch protocol -- the same way the other PG
    // integration tests apply full migration files.
    conn.batch_execute(CREATE_SCHEMA)
        .await
        .expect("create reddit schema");
    conn.batch_execute(ADD_AVATAR)
        .await
        .expect("add users.avatar column");
    conn.batch_execute(CREATE_TAGS)
        .await
        .expect("create tags/post_tags");
}

/// Seed one user, one subreddit, and one post by `author_id = 1`.
async fn seed_base(conn: &mut AsyncPgConnection) {
    diesel::sql_query("INSERT INTO users (username, password_hash) VALUES ('ada', 'h')")
        .execute(conn)
        .await
        .expect("seed users");
    diesel::sql_query(
        "INSERT INTO subreddits (name, slug, description, creator_id) VALUES \
         ('rust', 'rust', 'systems', 1)",
    )
    .execute(conn)
    .await
    .expect("seed subreddit");
    diesel::sql_query(
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id) VALUES \
         ('hello', 'hello', 'body', 1, 1)",
    )
    .execute(conn)
    .await
    .expect("seed post");
}

async fn insert_tag(conn: &mut AsyncPgConnection, name: &str, slug: &str) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
    diesel::sql_query("INSERT INTO tags (name, slug) VALUES ($1, $2) RETURNING id")
        .bind::<diesel::sql_types::Text, _>(name)
        .bind::<diesel::sql_types::Text, _>(slug)
        .get_result::<IdRow>(conn)
        .await
        .expect("seed tag")
        .id
}

async fn link_tag(conn: &mut AsyncPgConnection, post_id: i64, tag_id: i64) {
    diesel::sql_query("INSERT INTO post_tags (post_id, tag_id) VALUES ($1, $2)")
        .bind::<BigInt, _>(post_id)
        .bind::<BigInt, _>(tag_id)
        .execute(conn)
        .await
        .expect("seed post_tags row");
}

async fn all_posts(conn: &mut AsyncPgConnection) -> Vec<Preloaded<Post>> {
    posts::table
        .order(posts::id.asc())
        .select(Post::as_select())
        .load::<Post>(conn)
        .await
        .expect("load posts")
        .into_iter()
        .map(Preloaded::new)
        .collect()
}

async fn post_tags_count(conn: &mut AsyncPgConnection, post_id: i64) -> i64 {
    diesel::sql_query("SELECT COUNT(*)::BIGINT AS count FROM post_tags WHERE post_id = $1")
        .bind::<BigInt, _>(post_id)
        .get_result::<CountRow>(conn)
        .await
        .expect("count post_tags")
        .count
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn m2m_preload_happy_path() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    seed_base(&mut conn).await;
    let rust_id = insert_tag(&mut conn, "Rust", "rust-lang").await;
    let web_id = insert_tag(&mut conn, "Web", "web").await;
    link_tag(&mut conn, 1, rust_id).await;
    link_tag(&mut conn, 1, web_id).await;

    let mut loaded = all_posts(&mut conn).await;
    <Post as Preloadable>::load_associations(&mut loaded, &Post::preload().tags(), &mut conn)
        .await
        .expect("preload");

    let tags = loaded[0].tags().expect("tags preloaded");
    assert_eq!(tags.len(), 2);
    let slugs: Vec<&str> = tags.iter().map(|t| t.slug.as_str()).collect();
    assert!(slugs.contains(&"rust-lang"));
    assert!(slugs.contains(&"web"));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn m2m_preload_empty_is_empty_slice() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    seed_base(&mut conn).await; // post with zero tags

    let mut loaded = all_posts(&mut conn).await;
    <Post as Preloadable>::load_associations(&mut loaded, &Post::preload().tags(), &mut conn)
        .await
        .expect("preload");

    let tags = loaded[0].tags().expect("tags preloaded (empty)");
    assert!(tags.is_empty(), "no tags => empty slice, not error");
}

/// No Docker: accessing an un-preloaded association is a typed error, not SQL.
#[test]
fn m2m_not_preloaded_is_typed_error() {
    let post = Preloaded::new(Post {
        id: 1,
        title: "t".into(),
        slug: "t".into(),
        body: "b".into(),
        url: None,
        author_id: 1,
        subreddit_id: 1,
        score: 0,
        hot_rank: 0.0,
        comment_count: 0,
        created_at: chrono::NaiveDateTime::default(),
        updated_at: chrono::NaiveDateTime::default(),
    });
    let err = post.tags().expect_err("must be NotLoaded");
    assert_eq!(err.model, "Post");
    assert_eq!(err.association, "tags");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn m2m_nested_through_path() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    seed_base(&mut conn).await;
    diesel::sql_query(
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id) VALUES \
         ('second', 'second', 'body', 1, 1)",
    )
    .execute(&mut conn)
    .await
    .expect("seed second post");
    let rust_id = insert_tag(&mut conn, "Rust", "rust-lang").await;
    link_tag(&mut conn, 1, rust_id).await; // "hello"
    link_tag(&mut conn, 2, rust_id).await; // "second"

    let mut loaded = all_posts(&mut conn).await;
    // posts.preload(tags.posts) -- the mirror-image `through =` association.
    <Post as Preloadable>::load_associations(
        &mut loaded,
        &Post::preload().tags_with(Tag::preload().posts()),
        &mut conn,
    )
    .await
    .expect("preload nested");

    let tags = loaded[0].tags().expect("tags");
    assert_eq!(tags.len(), 1);
    let nested_posts = tags[0].posts().expect("nested posts preloaded");
    assert_eq!(
        nested_posts.len(),
        2,
        "the Rust tag is linked to both seeded posts"
    );
}

/// The number of SQL statements a `tags()` preload issues does not grow with
/// the number of tags on a post -- the core "no N+1" guarantee, exercised
/// through a join table rather than a plain fk column.
#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn m2m_preload_is_batched_no_n_plus_one() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    let mut meter = pool.get().await.expect("meter conn");
    setup_schema(&mut conn).await;
    diesel::sql_query("INSERT INTO users (username, password_hash) VALUES ('ada','h')")
        .execute(&mut conn)
        .await
        .unwrap();
    diesel::sql_query(
        "INSERT INTO subreddits (name, slug, description, creator_id) VALUES ('r','r','d',1)",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    diesel::sql_query(
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id) VALUES \
         ('small','small','b',1,1), ('big','big','b',1,1)",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    // 2 tags on post 1 ("small"), 40 on post 2 ("big").
    diesel::sql_query(
        "INSERT INTO tags (name, slug) SELECT 'tag' || n, 'tag-' || n \
         FROM generate_series(1, 40) AS n",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    diesel::sql_query(
        "INSERT INTO post_tags (post_id, tag_id) \
         SELECT 1, id FROM tags WHERE name IN ('tag1', 'tag2')",
    )
    .execute(&mut conn)
    .await
    .unwrap();
    diesel::sql_query("INSERT INTO post_tags (post_id, tag_id) SELECT 2, id FROM tags")
        .execute(&mut conn)
        .await
        .unwrap();

    async fn statement_count(meter: &mut AsyncPgConnection) -> i64 {
        diesel::sql_query(
            "SELECT (xact_commit + xact_rollback)::BIGINT AS count \
             FROM pg_stat_database WHERE datname = current_database()",
        )
        .get_result::<CountRow>(meter)
        .await
        .expect("read stat")
        .count
    }

    // Small post.
    let mut small: Vec<Preloaded<Post>> = vec![Preloaded::new(
        posts::table
            .filter(posts::slug.eq("small"))
            .select(Post::as_select())
            .first::<Post>(&mut conn)
            .await
            .unwrap(),
    )];
    let before = statement_count(&mut meter).await;
    <Post as Preloadable>::load_associations(&mut small, &Post::preload().tags(), &mut conn)
        .await
        .unwrap();
    let after = statement_count(&mut meter).await;
    let small_delta = after - before;

    // Big post (20x the tags).
    let mut big: Vec<Preloaded<Post>> = vec![Preloaded::new(
        posts::table
            .filter(posts::slug.eq("big"))
            .select(Post::as_select())
            .first::<Post>(&mut conn)
            .await
            .unwrap(),
    )];
    let before = statement_count(&mut meter).await;
    <Post as Preloadable>::load_associations(&mut big, &Post::preload().tags(), &mut conn)
        .await
        .unwrap();
    let after = statement_count(&mut meter).await;
    let big_delta = after - before;

    assert_eq!(big[0].tags().unwrap().len(), 40);
    assert_eq!(small[0].tags().unwrap().len(), 2);
    assert_eq!(
        small_delta, big_delta,
        "m2m preload statement count must not grow with tag count (no N+1): \
         small={small_delta}, big={big_delta}"
    );
    // tags (single INNER JOIN ... IN) = 1 statement.
    assert!(
        small_delta <= 2,
        "expected a single batched join statement, got {small_delta}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn add_tag_is_idempotent() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    seed_base(&mut conn).await;
    let tag_id = insert_tag(&mut conn, "Rust", "rust-lang").await;

    let repo = PgPostRepository::with_pool_untracked(pool.clone());
    repo.add_tag(1, tag_id).await.expect("first add");
    repo.add_tag(1, tag_id)
        .await
        .expect("duplicate add is a no-op, not an error");

    assert_eq!(post_tags_count(&mut conn, 1).await, 1);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn remove_tag_unlinks() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    seed_base(&mut conn).await;
    let tag_id = insert_tag(&mut conn, "Rust", "rust-lang").await;
    link_tag(&mut conn, 1, tag_id).await;

    let repo = PgPostRepository::with_pool_untracked(pool.clone());
    repo.remove_tag(1, tag_id).await.expect("remove");
    assert_eq!(post_tags_count(&mut conn, 1).await, 0);

    // Removing an already-unlinked pair is also a no-op, not an error.
    repo.remove_tag(1, tag_id)
        .await
        .expect("removing an unlinked pair is a no-op");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn set_tags_replaces_all() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    seed_base(&mut conn).await;
    let rust_id = insert_tag(&mut conn, "Rust", "rust-lang").await;
    let web_id = insert_tag(&mut conn, "Web", "web").await;
    let db_id = insert_tag(&mut conn, "Databases", "databases").await;
    link_tag(&mut conn, 1, rust_id).await;
    link_tag(&mut conn, 1, web_id).await;

    let repo = PgPostRepository::with_pool_untracked(pool.clone());
    // Replace {rust, web} with {databases} in one call.
    repo.set_tags(1, &[db_id])
        .await
        .expect("set_tags replaces the full set");

    let mut loaded = all_posts(&mut conn).await;
    <Post as Preloadable>::load_associations(&mut loaded, &Post::preload().tags(), &mut conn)
        .await
        .expect("preload");
    let tags = loaded[0].tags().expect("tags preloaded");
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].slug, "databases");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn set_tags_dedupes_input() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    seed_base(&mut conn).await;
    let tag_id = insert_tag(&mut conn, "Rust", "rust-lang").await;

    let repo = PgPostRepository::with_pool_untracked(pool.clone());
    repo.set_tags(1, &[tag_id, tag_id, tag_id])
        .await
        .expect("set_tags with duplicate ids");

    assert_eq!(post_tags_count(&mut conn, 1).await, 1);
}
