//! Integration tests: declarative associations + eager loading (`preload`).
//!
//! Covers issue #835's test ACs against a real Postgres:
//!
//! 1. `preload_happy_path` – `belongs_to` author + `has_many` comments load and
//!    read back through the typed accessors.
//! 2. `preload_empty_children` – a post with no comments yields an empty slice,
//!    not an error.
//! 3. `preload_missing_parent` – a `belongs_to` whose row was deleted reads as
//!    `Ok(None)` (preloaded-but-absent), distinct from `NotLoaded`.
//! 4. `preload_nested_path` – `comments.author` (a nested `belongs_to`) loads.
//! 5. `accessing_not_preloaded_is_typed_error` – an un-preloaded accessor
//!    returns `NotLoaded`, never SQL. (No Docker needed.)
//! 6. `preload_is_batched_no_n_plus_one` – the number of SQL statements a
//!    preload issues is independent of the child result-set size: a post with
//!    2 comments and a post with 40 comments issue the *same* number of
//!    statements. This is the success metric's "1 + K, independent of
//!    result-set size".
//!
//! The Docker-backed tests are `#[ignore]` (like the other PG integration
//! tests). Run them with:
//!
//! ```text
//! cargo test -p reddit-clone --test preload_pg_integration -- --ignored
//! ```

use autumn_web::preload::{Preloadable, Preloaded};
use diesel::prelude::*;
use diesel::sql_types::BigInt;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};

use reddit_clone::models::{Comment, CommentAssociations, Post, PostAssociations};
use reddit_clone::schema::posts;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const CREATE_SCHEMA: &str = include_str!("../migrations/20260419000000_create_reddit/up.sql");
// `User::as_select()` (used by `author` preloads) includes the `avatar`
// column added by this later migration, so apply it too.
const ADD_AVATAR: &str = include_str!("../migrations/20260427000000_add_user_avatar/up.sql");

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

async fn start_postgres() -> (impl std::any::Any, Pool<AsyncPgConnection>) {
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
    (container, pool)
}

async fn setup_schema(conn: &mut AsyncPgConnection) {
    // The migration files contain many semicolon-separated statements; Postgres
    // rejects multiple commands in a prepared statement, so use the simple
    // (unprepared) batch protocol — the same way the other PG integration
    // tests apply full migration files.
    conn.batch_execute(CREATE_SCHEMA)
        .await
        .expect("create reddit schema");
    conn.batch_execute(ADD_AVATAR)
        .await
        .expect("add users.avatar column");
}

/// Seed two users, one subreddit, and one post by `author_id = 1`.
async fn seed_base(conn: &mut AsyncPgConnection) {
    diesel::sql_query(
        "INSERT INTO users (username, password_hash) VALUES \
         ('ada', 'h'), ('grace', 'h')",
    )
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

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn preload_happy_path() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    seed_base(&mut conn).await;
    diesel::sql_query(
        "INSERT INTO comments (body, author_id, post_id) VALUES \
         ('first', 2, 1), ('second', 1, 1)",
    )
    .execute(&mut *conn)
    .await
    .expect("seed comments");

    let mut loaded = all_posts(&mut conn).await;
    <Post as Preloadable>::load_associations(
        &mut loaded,
        &Post::preload().author().comments(),
        &mut conn,
    )
    .await
    .expect("preload");

    let post = &loaded[0];
    let author = post
        .author()
        .expect("author preloaded")
        .expect("author present");
    assert_eq!(author.username, "ada");

    let comments = post.comments().expect("comments preloaded");
    assert_eq!(comments.len(), 2);
    let bodies: Vec<&str> = comments.iter().map(|c| c.body.as_str()).collect();
    assert!(bodies.contains(&"first"));
    assert!(bodies.contains(&"second"));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn preload_empty_children() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    seed_base(&mut conn).await; // post with zero comments

    let mut loaded = all_posts(&mut conn).await;
    <Post as Preloadable>::load_associations(&mut loaded, &Post::preload().comments(), &mut conn)
        .await
        .expect("preload");

    let comments = loaded[0].comments().expect("comments preloaded (empty)");
    assert!(comments.is_empty(), "no comments => empty slice, not error");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn preload_missing_parent() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    // `posts.author_id` is `NOT NULL REFERENCES users(id)`, so an orphan row
    // can't exist under the strict FK. Drop just that constraint for this case
    // to exercise the "preloaded but parent missing" → `Ok(None)` path.
    diesel::sql_query("ALTER TABLE posts DROP CONSTRAINT IF EXISTS posts_author_id_fkey")
        .execute(&mut *conn)
        .await
        .expect("drop posts.author_id fk");
    // A post whose author_id points at a user that does not exist.
    diesel::sql_query("INSERT INTO users (username, password_hash) VALUES ('ada', 'h')")
        .execute(&mut *conn)
        .await
        .unwrap();
    diesel::sql_query(
        "INSERT INTO subreddits (name, slug, description, creator_id) VALUES ('r','r','d',1)",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    diesel::sql_query(
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id) VALUES \
         ('orphan', 'orphan', 'b', 9999, 1)",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    let mut loaded = all_posts(&mut conn).await;
    <Post as Preloadable>::load_associations(&mut loaded, &Post::preload().author(), &mut conn)
        .await
        .expect("preload");

    let author = loaded[0].author().expect("author was preloaded");
    assert!(
        author.is_none(),
        "missing parent => Ok(None), distinct from NotLoaded"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn preload_nested_path() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    seed_base(&mut conn).await;
    diesel::sql_query("INSERT INTO comments (body, author_id, post_id) VALUES ('hi', 2, 1)")
        .execute(&mut *conn)
        .await
        .unwrap();

    let mut loaded = all_posts(&mut conn).await;
    // posts.preload(comments.author)
    <Post as Preloadable>::load_associations(
        &mut loaded,
        &Post::preload().comments_with(Comment::preload().author()),
        &mut conn,
    )
    .await
    .expect("preload nested");

    let comments = loaded[0].comments().expect("comments");
    let comment_author = comments[0]
        .author()
        .expect("nested author preloaded")
        .expect("author present");
    assert_eq!(comment_author.username, "grace");
}

/// No Docker: accessing an un-preloaded association is a typed error, not SQL.
#[test]
fn accessing_not_preloaded_is_typed_error() {
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
    let err = post.author().expect_err("must be NotLoaded");
    assert_eq!(err.model, "Post");
    assert_eq!(err.association, "author");
    assert!(post.comments().is_err());
}

/// The number of SQL statements a preload issues does not grow with the child
/// result-set size — the core "no N+1" guarantee. We preload the same spec for
/// a post with 2 comments and a post with 40 comments and assert the statement
/// delta is identical (and small).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn preload_is_batched_no_n_plus_one() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    let mut meter = pool.get().await.expect("meter conn");
    setup_schema(&mut conn).await;
    diesel::sql_query("INSERT INTO users (username, password_hash) VALUES ('ada','h'),('bo','h')")
        .execute(&mut *conn)
        .await
        .unwrap();
    diesel::sql_query(
        "INSERT INTO subreddits (name, slug, description, creator_id) VALUES ('r','r','d',1)",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    diesel::sql_query(
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id) VALUES \
         ('small','small','b',1,1), ('big','big','b',1,1)",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    // 2 comments on post 1, 40 on post 2.
    diesel::sql_query(
        "INSERT INTO comments (body, author_id, post_id) \
         SELECT 'c', 2, 1 FROM generate_series(1, 2)",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    diesel::sql_query(
        "INSERT INTO comments (body, author_id, post_id) \
         SELECT 'c', 2, 2 FROM generate_series(1, 40)",
    )
    .execute(&mut *conn)
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

    let spec = || {
        Post::preload()
            .author()
            .comments_with(Comment::preload().author())
    };

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
    <Post as Preloadable>::load_associations(&mut small, &spec(), &mut conn)
        .await
        .unwrap();
    let after = statement_count(&mut meter).await;
    let small_delta = after - before;

    // Big post (20x the comments).
    let mut big: Vec<Preloaded<Post>> = vec![Preloaded::new(
        posts::table
            .filter(posts::slug.eq("big"))
            .select(Post::as_select())
            .first::<Post>(&mut conn)
            .await
            .unwrap(),
    )];
    let before = statement_count(&mut meter).await;
    <Post as Preloadable>::load_associations(&mut big, &spec(), &mut conn)
        .await
        .unwrap();
    let after = statement_count(&mut meter).await;
    let big_delta = after - before;

    assert_eq!(big[0].comments().unwrap().len(), 40);
    assert_eq!(small[0].comments().unwrap().len(), 2);
    assert_eq!(
        small_delta, big_delta,
        "preload statement count must not grow with child count (no N+1): \
         small={small_delta}, big={big_delta}"
    );
    // author (IN) + comments (IN) + comments.author (IN) = 3 statements.
    assert!(
        small_delta <= 4,
        "expected a bounded number of batched statements, got {small_delta}"
    );
}
