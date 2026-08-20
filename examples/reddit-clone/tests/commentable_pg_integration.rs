//! Integration tests: the polymorphic `#[commentable]` association (#1367) in
//! the example app, against a real Postgres.
//!
//! This is the end-to-end proof of the issue's success metric. reddit-clone's
//! 188-line hand-rolled `src/routes/comments.rs` and its `post_id`-keyed
//! `comments` table are gone; what replaced them is one attribute per model:
//!
//! ```rust,ignore
//! #[commentable(by = User, author_name = username)]
//! pub struct Post { /* … */ }
//!
//! #[commentable(by = User, author_name = username)]
//! pub struct Subreddit { /* … */ }
//! ```
//!
//! 1. `post_comments_thread_and_maintain_the_counter` — AC3 + AC6: replies nest
//!    under their parent in stable order, and `posts.comment_count` moves in
//!    the same transaction as the insert.
//! 2. `a_second_model_shares_the_one_comments_table` — AC1 + AC5: `Subreddit`
//!    comments land in the **same** physical table, told apart only by
//!    `commentable_type`, and neither model can see the other's thread. The
//!    second model added no table, no route, and no query.
//! 3. `deleting_a_comment_cascades_to_its_replies` — AC3 + AC6: the subtree goes
//!    with it and the counter drops by exactly the number of rows removed.
//! 4. `a_reply_cannot_be_grafted_onto_another_record` — the polymorphic key
//!    carries no foreign key, so the write path is the referential check.
//!
//! The Docker-backed tests are `#[ignore]` (like the other PG integration
//! tests) and run via testcontainers by default. When Docker is unavailable,
//! point them at an already-running Postgres instead:
//!
//! ```text
//! AUTUMN_TEST_PG_URL=postgres://autumn:autumn@127.0.0.1:5432/reddit \
//!   cargo test -p reddit-clone --test commentable_pg_integration -- --ignored --test-threads=1
//! ```

use diesel::sql_types::{BigInt, Text};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};

use autumn_web::commentable::CommentNode;
// The traits `#[commentable]` emits on `Post` and `Subreddit`, bringing
// `add_comment` / `comment_thread` / `delete_comment` into scope for the
// generated repositories.
use reddit_clone::models::{Post, PostComments as _, Subreddit, SubredditComments as _};
use reddit_clone::repositories::{PgPostRepository, PgSubredditRepository};
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const CREATE_SCHEMA: &str = include_str!("../migrations/20260419000000_create_reddit/up.sql");
const ADD_AVATAR: &str = include_str!("../migrations/20260427000000_add_user_avatar/up.sql");
const POLYMORPHIC_COMMENTS: &str =
    include_str!("../migrations/20260820000000_polymorphic_comments/up.sql");

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

/// Keeps whichever backing Postgres alive for the duration of the test.
enum PgHandle {
    Container(#[allow(dead_code)] Box<ContainerAsync<Postgres>>),
    External,
}

async fn start_postgres() -> (PgHandle, Pool<AsyncPgConnection>) {
    if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
        let pool = Pool::builder(manager)
            .max_size(20)
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
        .max_size(20)
        .build()
        .expect("build pool");
    (PgHandle::Container(Box::new(container)), pool)
}

async fn setup_schema(conn: &mut AsyncPgConnection) {
    if std::env::var("AUTUMN_TEST_PG_URL").is_ok() {
        conn.batch_execute(
            "DROP TABLE IF EXISTS post_tags, tags, comments, votes, \
             live_feed_events, posts, subreddits, users CASCADE;",
        )
        .await
        .expect("reset reddit-clone tables");
    }
    // The migration files contain many semicolon-separated statements; Postgres
    // rejects multiple commands in a prepared statement, so use the simple
    // (unprepared) batch protocol.
    conn.batch_execute(CREATE_SCHEMA)
        .await
        .expect("create reddit schema");
    conn.batch_execute(ADD_AVATAR)
        .await
        .expect("add users.avatar column");
    conn.batch_execute(POLYMORPHIC_COMMENTS)
        .await
        .expect("make comments polymorphic");
}

async fn seed_user(conn: &mut AsyncPgConnection, username: &str) -> i64 {
    diesel::sql_query("INSERT INTO users (username, password_hash) VALUES ($1, 'h') RETURNING id")
        .bind::<Text, _>(username)
        .get_result::<IdRow>(conn)
        .await
        .expect("seed user")
        .id
}

async fn seed_subreddit(conn: &mut AsyncPgConnection, creator_id: i64) -> i64 {
    diesel::sql_query(
        "INSERT INTO subreddits (name, slug, description, creator_id) \
         VALUES ('rust', 'rust', 'systems', $1) RETURNING id",
    )
    .bind::<BigInt, _>(creator_id)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed subreddit")
    .id
}

async fn seed_post(conn: &mut AsyncPgConnection, author_id: i64, subreddit_id: i64) -> i64 {
    diesel::sql_query(
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id) \
         VALUES ('hello', 'hello', 'body', $1, $2) RETURNING id",
    )
    .bind::<BigInt, _>(author_id)
    .bind::<BigInt, _>(subreddit_id)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed post")
    .id
}

/// The maintained counter and the ground truth in **one** statement, so both
/// necessarily come from the same snapshot.
#[derive(diesel::QueryableByName)]
struct SnapshotRow {
    #[diesel(sql_type = BigInt)]
    persisted: i64,
    #[diesel(sql_type = BigInt)]
    ground_truth: i64,
}

async fn counter_snapshot(
    conn: &mut AsyncPgConnection,
    table: &str,
    commentable_type: &str,
    id: i64,
) -> SnapshotRow {
    diesel::sql_query(format!(
        "SELECT t.comment_count AS persisted, \
         (SELECT COUNT(*) FROM comments c \
           WHERE c.commentable_type = $1 AND c.commentable_id = t.id \
             AND c.deleted_at IS NULL)::BIGINT AS ground_truth \
         FROM {table} t WHERE t.id = $2"
    ))
    .bind::<Text, _>(commentable_type)
    .bind::<BigInt, _>(id)
    .get_result::<SnapshotRow>(conn)
    .await
    .expect("read counter snapshot")
}

/// Every row in the one shared table, live or not.
async fn total_comment_rows(conn: &mut AsyncPgConnection) -> i64 {
    diesel::sql_query("SELECT COUNT(*)::BIGINT AS count FROM comments")
        .get_result::<CountRow>(conn)
        .await
        .expect("count comments")
        .count
}

/// Flatten a nested thread to `(depth, body)` in render order.
fn flatten(nodes: &[CommentNode]) -> Vec<(usize, String)> {
    fn walk(nodes: &[CommentNode], out: &mut Vec<(usize, String)>) {
        for node in nodes {
            out.push((node.depth, node.comment.body.clone()));
            walk(&node.replies, out);
        }
    }
    let mut out = Vec::new();
    walk(nodes, &mut out);
    out
}

/// AC3 + AC6: threading, stable order, resolved author names, and a counter
/// that matches ground truth.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn post_comments_thread_and_maintain_the_counter() {
    let (_pg, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    let ada = seed_user(&mut conn, "ada").await;
    let grace = seed_user(&mut conn, "grace").await;
    let sub = seed_subreddit(&mut conn, ada).await;
    let post = seed_post(&mut conn, ada, sub).await;

    let posts = PgPostRepository::with_pool_untracked(pool.clone());
    let root = posts
        .add_comment(post, ada, "first!", None)
        .await
        .expect("root comment");
    posts
        .add_comment(post, grace, "a reply", Some(root.id))
        .await
        .expect("reply");
    posts
        .add_comment(post, grace, "second top-level", None)
        .await
        .expect("second root");

    let thread = posts.comment_thread(post).await.expect("thread");
    assert_eq!(
        flatten(&thread),
        vec![
            (0, "first!".to_owned()),
            (1, "a reply".to_owned()),
            (0, "second top-level".to_owned()),
        ]
    );
    // `author_name = username` is what lets the widget render names without an
    // N+1 lookup per node.
    assert_eq!(thread[0].comment.author_name.as_deref(), Some("ada"));
    assert_eq!(
        thread[0].replies[0].comment.author_name.as_deref(),
        Some("grace")
    );

    let snapshot = counter_snapshot(&mut conn, "posts", Post::COMMENTABLE_TYPE, post).await;
    assert_eq!(snapshot.persisted, 3);
    assert_eq!(snapshot.persisted, snapshot.ground_truth);
}

/// AC1 + AC5: the second commentable model shares the one table. Adding it cost
/// the attribute and a counter column — no table, no route, no query — and the
/// two threads cannot see each other.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_second_model_shares_the_one_comments_table() {
    let (_pg, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    let ada = seed_user(&mut conn, "ada").await;
    let sub = seed_subreddit(&mut conn, ada).await;
    let post = seed_post(&mut conn, ada, sub).await;

    let posts = PgPostRepository::with_pool_untracked(pool.clone());
    let subreddits = PgSubredditRepository::with_pool_untracked(pool.clone());

    posts
        .add_comment(post, ada, "on the post", None)
        .await
        .expect("post comment");
    subreddits
        .add_comment(sub, ada, "on the community", None)
        .await
        .expect("subreddit comment");

    assert_eq!(
        flatten(&posts.comment_thread(post).await.expect("post thread")),
        vec![(0, "on the post".to_owned())]
    );
    assert_eq!(
        flatten(
            &subreddits
                .comment_thread(sub)
                .await
                .expect("subreddit thread")
        ),
        vec![(0, "on the community".to_owned())]
    );

    // Both rows are in the SAME table — that is the whole point of the
    // polymorphic kind.
    assert_eq!(total_comment_rows(&mut conn).await, 2);
    assert_eq!(Post::COMMENTABLE_TYPE, "Post");
    assert_eq!(Subreddit::COMMENTABLE_TYPE, "Subreddit");
    assert_eq!(
        Post::commentable_spec().comments_table,
        Subreddit::commentable_spec().comments_table
    );

    // …and each model keeps its own counter.
    assert_eq!(
        counter_snapshot(&mut conn, "posts", Post::COMMENTABLE_TYPE, post)
            .await
            .persisted,
        1
    );
    assert_eq!(
        counter_snapshot(&mut conn, "subreddits", Subreddit::COMMENTABLE_TYPE, sub)
            .await
            .persisted,
        1
    );
}

/// AC3 + AC6: deleting a comment takes its replies with it, the counter drops
/// by exactly the number removed, and the thread read is soft-delete aware.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn deleting_a_comment_cascades_to_its_replies() {
    let (_pg, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    let ada = seed_user(&mut conn, "ada").await;
    let sub = seed_subreddit(&mut conn, ada).await;
    let post = seed_post(&mut conn, ada, sub).await;

    let posts = PgPostRepository::with_pool_untracked(pool.clone());
    let root = posts.add_comment(post, ada, "a", None).await.expect("a");
    let reply = posts
        .add_comment(post, ada, "a1", Some(root.id))
        .await
        .expect("a1");
    posts
        .add_comment(post, ada, "a1x", Some(reply.id))
        .await
        .expect("a1x");
    posts.add_comment(post, ada, "b", None).await.expect("b");
    assert_eq!(
        counter_snapshot(&mut conn, "posts", Post::COMMENTABLE_TYPE, post)
            .await
            .persisted,
        4
    );

    let removed = posts.delete_comment(root.id).await.expect("delete");
    assert_eq!(removed, 3, "the root plus both descendants");

    let snapshot = counter_snapshot(&mut conn, "posts", Post::COMMENTABLE_TYPE, post).await;
    assert_eq!(snapshot.persisted, 1);
    assert_eq!(snapshot.persisted, snapshot.ground_truth);
    assert_eq!(
        flatten(&posts.comment_thread(post).await.expect("thread")),
        vec![(0, "b".to_owned())]
    );
    // Soft, not hard: the rows are still there, just not live.
    assert_eq!(total_comment_rows(&mut conn).await, 4);
}

/// `commentable_id` carries no foreign key — a single column cannot reference
/// two tables — so the write path is the referential check. A reply addressed
/// at another record's comment must not be grafted on.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_reply_cannot_be_grafted_onto_another_record() {
    let (_pg, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    let ada = seed_user(&mut conn, "ada").await;
    let sub = seed_subreddit(&mut conn, ada).await;
    let post = seed_post(&mut conn, ada, sub).await;

    let posts = PgPostRepository::with_pool_untracked(pool.clone());
    let subreddits = PgSubredditRepository::with_pool_untracked(pool.clone());
    let on_post = posts
        .add_comment(post, ada, "root", None)
        .await
        .expect("root");

    let err = subreddits
        .add_comment(sub, ada, "graft", Some(on_post.id))
        .await
        .expect_err("a cross-record reply must be rejected");
    assert_eq!(err.status().as_u16(), 422);

    let err = posts
        .add_comment(9_999_999, ada, "ghost", None)
        .await
        .expect_err("an unknown parent must be rejected");
    assert_eq!(err.status().as_u16(), 404);

    assert_eq!(total_comment_rows(&mut conn).await, 1);
}
