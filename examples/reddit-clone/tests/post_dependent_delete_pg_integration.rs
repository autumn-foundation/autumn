//! PostgreSQL proof for the reddit-clone's real `Post` dependent graph.
//! The success case covers destroy-vs-bulk semantics and nullable vote targets;
//! the failure case proves the generated cascade is one atomic transaction.

use diesel::sql_types::BigInt;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};
use reddit_clone::repositories::{PgPostRepository, PostRepository};
use std::sync::{Arc, OnceLock};
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;
use tokio::sync::{Mutex, OwnedMutexGuard};

const CREATE_SCHEMA: &str = include_str!("../migrations/20260419000000_create_reddit/up.sql");
const ADD_AVATAR: &str = include_str!("../migrations/20260427000000_add_user_avatar/up.sql");

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

enum PgHandle {
    Container(#[allow(dead_code)] Box<ContainerAsync<Postgres>>),
    // An external URL points every test at the same persistent database. Keep
    // this process-wide guard for the entire test, not merely schema setup, so
    // parallel tests cannot drop one another's tables midway through a query.
    External(#[allow(dead_code)] OwnedMutexGuard<()>),
}

async fn external_database_guard() -> OwnedMutexGuard<()> {
    static EXTERNAL_DATABASE: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
    EXTERNAL_DATABASE
        .get_or_init(|| Arc::new(Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

async fn setup() -> (PgHandle, Pool<AsyncPgConnection>) {
    let (handle, url) = if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
        (PgHandle::External(external_database_guard().await), url)
    } else {
        let container = Postgres::default().start().await.expect("start Postgres");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("Postgres port");
        (
            PgHandle::Container(Box::new(container)),
            format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres"),
        )
    };
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
    let pool = Pool::builder(manager)
        .max_size(4)
        .build()
        .expect("build pool");
    let mut conn = pool.get().await.expect("connection");
    if std::env::var("AUTUMN_TEST_PG_URL").is_ok() {
        conn.batch_execute(
             "DROP TABLE IF EXISTS post_tags, tags, comments, votes, live_feed_events, \
             posts, subreddits, users CASCADE;
             DROP FUNCTION IF EXISTS reject_comment_delete();
             DROP FUNCTION IF EXISTS reject_parent_before_reply();",
        )
        .await
        .expect("reset schema");
    }
    conn.batch_execute(CREATE_SCHEMA)
        .await
        .expect("create schema");
    conn.batch_execute(ADD_AVATAR).await.expect("add avatar");
    drop(conn);
    (handle, pool)
}

#[tokio::test]
async fn external_database_guard_serializes_tests() {
    let first = external_database_guard().await;
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            external_database_guard()
        )
        .await
        .is_err(),
        "a second external-database test must wait while the first is running"
    );

    drop(first);
    tokio::time::timeout(std::time::Duration::from_secs(1), external_database_guard())
        .await
        .expect("the next test can acquire the released external-database guard");
}

async fn seed_graph(conn: &mut AsyncPgConnection) {
    conn.batch_execute(
        "INSERT INTO users (id, username, password_hash) VALUES
           (1, 'author', 'h'), (2, 'voter-a', 'h'), (3, 'voter-b', 'h');
         INSERT INTO subreddits (id, name, slug, creator_id)
           VALUES (1, 'Rust', 'rust', 1);
         INSERT INTO posts (id, title, slug, author_id, subreddit_id, comment_count, score)
           VALUES (10, 'target', 'target', 1, 1, 3, 2),
                  (11, 'control', 'control', 1, 1, 0, 1);
         INSERT INTO comments (id, body, author_id, commentable_type, commentable_id, parent_id)
           VALUES (20, 'first', 1, 'Post', 10, NULL), (21, 'second', 1, 'Post', 10, NULL),
                  (22, 'nested reply', 1, 'Post', 10, 20);
         INSERT INTO votes (id, user_id, post_id, comment_id, value) VALUES
           (30, 2, 10, NULL, 1), (31, 3, 10, NULL, 1),
           (32, 2, NULL, 20, 1), (33, 3, NULL, 21, -1),
           (34, 2, 11, NULL, 1), (35, 2, NULL, 22, 1);",
    )
    .await
    .expect("seed post graph");
}

async fn count(conn: &mut AsyncPgConnection, sql: &str) -> i64 {
    diesel::sql_query(sql)
        .get_result::<CountRow>(conn)
        .await
        .expect("count query")
        .count
}

#[tokio::test]
#[ignore = "requires PostgreSQL (testcontainers or AUTUMN_TEST_PG_URL)"]
async fn repository_delete_removes_only_the_target_post_graph() {
    let (_handle, pool) = setup().await;
    let mut conn = pool.get().await.expect("connection");
    seed_graph(&mut conn).await;
    // This trigger turns repository ordering into an observable invariant: a
    // direct database cascade from comment 20 would encounter its live reply
    // and fail, whereas `Comment.replies dependent = destroy` removes 22 via
    // `PgCommentRepository` before deleting 20.
    conn.batch_execute(
        "CREATE OR REPLACE FUNCTION reject_parent_before_reply() RETURNS trigger
           LANGUAGE plpgsql AS $$
           BEGIN
             IF EXISTS (SELECT 1 FROM comments WHERE parent_id = OLD.id) THEN
               RAISE EXCEPTION 'reply lifecycle was bypassed';
             END IF;
             RETURN OLD;
           END $$;
         CREATE TRIGGER reject_parent_before_reply BEFORE DELETE ON comments
           FOR EACH ROW EXECUTE FUNCTION reject_parent_before_reply();",
    )
    .await
    .expect("install nested-comment ordering trigger");
    drop(conn);

    PgPostRepository::with_pool_untracked(pool.clone())
        .delete_by_id(10)
        .await
        .expect("delete post graph");

    let mut conn = pool.get().await.expect("connection");
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS count FROM posts WHERE id = 10"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS count FROM comments \
             WHERE commentable_type = 'Post' AND commentable_id = 10"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS count FROM votes WHERE post_id = 10"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS count FROM votes WHERE comment_id IN (20, 21, 22)"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT comment_count AS count FROM posts WHERE id = 11"
        )
        .await,
        0
    );
    assert_eq!(
        count(&mut conn, "SELECT score AS count FROM posts WHERE id = 11").await,
        1
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS count FROM votes WHERE post_id = 11"
        )
        .await,
        1
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL (testcontainers or AUTUMN_TEST_PG_URL)"]
async fn dependent_failure_rolls_back_the_complete_post_graph() {
    let (_handle, pool) = setup().await;
    let mut conn = pool.get().await.expect("connection");
    seed_graph(&mut conn).await;
    conn.batch_execute(
        "CREATE OR REPLACE FUNCTION reject_comment_delete() RETURNS trigger LANGUAGE plpgsql AS $$
           BEGIN RAISE EXCEPTION 'forced dependent failure'; END $$;
         CREATE TRIGGER reject_comment_delete BEFORE DELETE ON comments
           FOR EACH ROW EXECUTE FUNCTION reject_comment_delete();",
    )
    .await
    .expect("install failure trigger");
    drop(conn);

    let result = PgPostRepository::with_pool_untracked(pool.clone())
        .delete_by_id(10)
        .await;
    assert!(
        result.is_err(),
        "the forced child failure must reach the caller"
    );

    let mut conn = pool.get().await.expect("connection");
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS count FROM posts WHERE id = 10"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT comment_count AS count FROM posts WHERE id = 10"
        )
        .await,
        3
    );
    assert_eq!(
        count(&mut conn, "SELECT score AS count FROM posts WHERE id = 10").await,
        2
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS count FROM comments \
             WHERE commentable_type = 'Post' AND commentable_id = 10"
        )
        .await,
        3
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS count FROM votes WHERE post_id = 10"
        )
        .await,
        2
    );
    assert_eq!(
        count(
            &mut conn,
            "SELECT COUNT(*) AS count FROM votes WHERE comment_id IN (20, 21, 22)"
        )
        .await,
        3
    );
}
