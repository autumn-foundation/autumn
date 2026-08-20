//! Integration tests: declarative associations + eager loading (`preload`).
//!
//! Covers issue #835's test ACs against a real Postgres:
//!
//! 1. `preload_happy_path` – `belongs_to` creator + `has_many` posts load and
//!    read back through the typed accessors.
//! 2. `preload_empty_children` – a subreddit with no posts yields an empty
//!    slice, not an error.
//! 3. `preload_missing_parent` – a `belongs_to` whose row was deleted reads as
//!    `Ok(None)` (preloaded-but-absent), distinct from `NotLoaded`.
//! 4. `preload_nested_path` – `posts.author` (a nested `belongs_to`) loads.
//! 5. `accessing_not_preloaded_is_typed_error` – an un-preloaded accessor
//!    returns `NotLoaded`, never SQL. (No Docker needed.)
//! 6. `preload_is_batched_no_n_plus_one` – the number of SQL statements a
//!    preload issues is independent of the child result-set size: a subreddit
//!    with 2 posts and one with 40 posts issue the *same* number of
//!    statements. This is the success metric's "1 + K, independent of
//!    result-set size".
//!
//! The fixture is `Subreddit -> Post -> User`. It used to be `Post ->
//! Comment -> User`, until #1367 made comments a *polymorphic* association
//! (one `comments` table keyed on `(commentable_type, commentable_id)`), which
//! by construction is not a `has_many` leg. Same three association shapes,
//! different tables.
//!
//! The Docker-backed tests are `#[ignore]` (like the other PG integration
//! tests). Run them with:
//!
//! ```text
//! cargo test -p reddit-clone --test preload_pg_integration -- --ignored
//! ```
//!
//! When Docker is unavailable, point them at an already-running Postgres:
//!
//! ```text
//! AUTUMN_TEST_PG_URL=postgres://autumn:autumn@127.0.0.1:5432/reddit \
//!   cargo test -p reddit-clone --test preload_pg_integration -- --ignored --test-threads=1
//! ```

use autumn_web::preload::{Preloadable, Preloaded};
use diesel::prelude::*;
use diesel::sql_types::BigInt;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};

use reddit_clone::models::{Post, PostAssociations, Subreddit, SubredditAssociations};
use reddit_clone::schema::subreddits;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const CREATE_SCHEMA: &str = include_str!("../migrations/20260419000000_create_reddit/up.sql");
// `User::as_select()` (used by `author`/`creator` preloads) includes the
// `avatar` column added by this later migration, so apply it too.
const ADD_AVATAR: &str = include_str!("../migrations/20260427000000_add_user_avatar/up.sql");
// `Subreddit::as_select()` includes the `comment_count` column this migration
// adds (#1367).
const POLYMORPHIC_COMMENTS: &str =
    include_str!("../migrations/20260820000000_polymorphic_comments/up.sql");

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

async fn start_postgres() -> (Box<dyn std::any::Any>, Pool<AsyncPgConnection>) {
    // The same `AUTUMN_TEST_PG_URL` escape hatch the votable/m2m suites carry,
    // for a checkout where Docker is unavailable. Run those with
    // `--test-threads=1`: each test resets the reddit-clone tables on the
    // shared external database before seeding.
    if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
        let pool = Pool::builder(manager)
            .max_size(4)
            .build()
            .expect("build pool");
        return (Box::new(()), pool);
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
    (Box::new(container), pool)
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
    // (unprepared) batch protocol — the same way the other PG integration
    // tests apply full migration files.
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

/// Seed two users, one subreddit created by user 1, and one post by
/// `author_id = 2`.
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
         ('hello', 'hello', 'body', 2, 1)",
    )
    .execute(conn)
    .await
    .expect("seed post");
}

async fn all_subreddits(conn: &mut AsyncPgConnection) -> Vec<Preloaded<Subreddit>> {
    subreddits::table
        .order(subreddits::id.asc())
        .select(Subreddit::as_select())
        .load::<Subreddit>(conn)
        .await
        .expect("load subreddits")
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
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id) VALUES \
         ('second', 'second', 'b', 1, 1)",
    )
    .execute(&mut *conn)
    .await
    .expect("seed second post");

    let mut loaded = all_subreddits(&mut conn).await;
    <Subreddit as Preloadable>::load_associations(
        &mut loaded,
        &Subreddit::preload().creator().posts(),
        &mut conn,
    )
    .await
    .expect("preload");

    let sub = &loaded[0];
    let creator = sub
        .creator()
        .expect("creator preloaded")
        .expect("creator present");
    assert_eq!(creator.username, "ada");

    let posts = sub.posts().expect("posts preloaded");
    assert_eq!(posts.len(), 2);
    let titles: Vec<&str> = posts.iter().map(|p| p.title.as_str()).collect();
    assert!(titles.contains(&"hello"));
    assert!(titles.contains(&"second"));
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn preload_empty_children() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    diesel::sql_query("INSERT INTO users (username, password_hash) VALUES ('ada', 'h')")
        .execute(&mut *conn)
        .await
        .expect("seed user");
    // A subreddit with zero posts.
    diesel::sql_query(
        "INSERT INTO subreddits (name, slug, description, creator_id) VALUES \
         ('empty', 'empty', 'd', 1)",
    )
    .execute(&mut *conn)
    .await
    .expect("seed subreddit");

    let mut loaded = all_subreddits(&mut conn).await;
    <Subreddit as Preloadable>::load_associations(
        &mut loaded,
        &Subreddit::preload().posts(),
        &mut conn,
    )
    .await
    .expect("preload");

    let posts = loaded[0].posts().expect("posts preloaded (empty)");
    assert!(posts.is_empty(), "no posts => empty slice, not error");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn preload_missing_parent() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    // `subreddits.creator_id` is `NOT NULL REFERENCES users(id)`, so an orphan
    // row can't exist under the strict FK. Drop just that constraint for this
    // case to exercise the "preloaded but parent missing" → `Ok(None)` path.
    diesel::sql_query("ALTER TABLE subreddits DROP CONSTRAINT IF EXISTS subreddits_creator_id_fkey")
        .execute(&mut *conn)
        .await
        .expect("drop subreddits.creator_id fk");
    // A subreddit whose creator_id points at a user that does not exist.
    diesel::sql_query(
        "INSERT INTO subreddits (name, slug, description, creator_id) VALUES \
         ('orphan', 'orphan', 'd', 9999)",
    )
    .execute(&mut *conn)
    .await
    .unwrap();

    let mut loaded = all_subreddits(&mut conn).await;
    <Subreddit as Preloadable>::load_associations(
        &mut loaded,
        &Subreddit::preload().creator(),
        &mut conn,
    )
    .await
    .expect("preload");

    let creator = loaded[0].creator().expect("creator was preloaded");
    assert!(
        creator.is_none(),
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

    let mut loaded = all_subreddits(&mut conn).await;
    // subreddits.preload(posts.author)
    <Subreddit as Preloadable>::load_associations(
        &mut loaded,
        &Subreddit::preload().posts_with(Post::preload().author()),
        &mut conn,
    )
    .await
    .expect("preload nested");

    let posts = loaded[0].posts().expect("posts");
    let post_author = posts[0]
        .author()
        .expect("nested author preloaded")
        .expect("author present");
    assert_eq!(post_author.username, "grace");
}

/// No Docker: accessing an un-preloaded association is a typed error, not SQL.
#[test]
fn accessing_not_preloaded_is_typed_error() {
    let sub = Preloaded::new(Subreddit {
        id: 1,
        name: "rust".into(),
        slug: "rust".into(),
        description: "d".into(),
        creator_id: 1,
        subscriber_count: 0,
        comment_count: 0,
        created_at: chrono::NaiveDateTime::default(),
    });
    let err = sub.creator().expect_err("must be NotLoaded");
    assert_eq!(err.model, "Subreddit");
    assert_eq!(err.association, "creator");
    assert!(sub.posts().is_err());
}

/// The number of SQL statements a preload issues does not grow with the child
/// result-set size — the core "no N+1" guarantee. We preload the same spec for
/// a subreddit with 2 posts and one with 40 posts and assert the statement
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
        "INSERT INTO subreddits (name, slug, description, creator_id) VALUES \
         ('small','small','d',1), ('big','big','d',1)",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    // 2 posts in subreddit 1, 40 in subreddit 2. Slugs are unique per row so
    // the seed cannot collide on `posts.slug`.
    diesel::sql_query(
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id) \
         SELECT 't', 'small-' || i, 'b', 2, 1 FROM generate_series(1, 2) AS g(i)",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    diesel::sql_query(
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id) \
         SELECT 't', 'big-' || i, 'b', 2, 2 FROM generate_series(1, 40) AS g(i)",
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
        Subreddit::preload()
            .creator()
            .posts_with(Post::preload().author())
    };

    // Small subreddit.
    let mut small: Vec<Preloaded<Subreddit>> = vec![Preloaded::new(
        subreddits::table
            .filter(subreddits::slug.eq("small"))
            .select(Subreddit::as_select())
            .first::<Subreddit>(&mut conn)
            .await
            .unwrap(),
    )];
    let before = statement_count(&mut meter).await;
    <Subreddit as Preloadable>::load_associations(&mut small, &spec(), &mut conn)
        .await
        .unwrap();
    let after = statement_count(&mut meter).await;
    let small_delta = after - before;

    // Big subreddit (20x the posts).
    let mut big: Vec<Preloaded<Subreddit>> = vec![Preloaded::new(
        subreddits::table
            .filter(subreddits::slug.eq("big"))
            .select(Subreddit::as_select())
            .first::<Subreddit>(&mut conn)
            .await
            .unwrap(),
    )];
    let before = statement_count(&mut meter).await;
    <Subreddit as Preloadable>::load_associations(&mut big, &spec(), &mut conn)
        .await
        .unwrap();
    let after = statement_count(&mut meter).await;
    let big_delta = after - before;

    assert_eq!(big[0].posts().unwrap().len(), 40);
    assert_eq!(small[0].posts().unwrap().len(), 2);
    assert_eq!(
        small_delta, big_delta,
        "preload statement count must not grow with child count (no N+1): \
         small={small_delta}, big={big_delta}"
    );
    // creator (IN) + posts (IN) + posts.author (IN) = 3 statements.
    assert!(
        small_delta <= 4,
        "expected a bounded number of batched statements, got {small_delta}"
    );
}
