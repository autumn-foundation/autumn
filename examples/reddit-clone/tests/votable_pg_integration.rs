//! Integration tests: the declarative `#[votable]` reaction association
//! (#1362) running against reddit-clone's **shipped** `votes` table and real
//! migrations — no new migration, no schema change.
//!
//! This is the end-to-end proof of the convention claim in the plan: every
//! `#[votable(by = User, aggregate = sum)]` default (`table = votes`,
//! `reactor_fk = user_id`, `target_fk = post_id`, `value_column = value`,
//! `column = score`) already matches the table the example has shipped since
//! its first migration, including the load-bearing
//! `CONSTRAINT votes_unique_post UNIQUE (user_id, post_id)` that the generated
//! upsert names as its `ON CONFLICT` arbiter.
//!
//! Covers the issue's acceptance criteria against a real Postgres:
//!
//! 1. `votable_react_upvotes_and_maintains_posts_score` -- AC2 + AC3: one
//!    `react()` call inserts the edge and persists `posts.score` equal to the
//!    ground-truth `SUM(votes.value)`, in one transaction.
//! 2. `votable_react_toggles_off` -- AC2: clicking the same direction twice
//!    deletes the edge and returns the score to zero.
//! 3. `votable_react_flips` -- AC2: the opposite direction replaces the edge in
//!    place; there is never a second row for the pair.
//! 4. `votable_react_is_race_safe_on_the_shipped_votes_table` -- AC2 and the
//!    issue's success metric: 50 simultaneous clicks on the same
//!    `(user, post)` pair are 50 serialized toggles, so they end at exactly
//!    zero edges with `posts.score` back at `0`, and no caller sees a
//!    unique-violation.
//! 5. `leaderboard_grouped_aggregate_still_works_after_react` -- the regression
//!    guard for #1364: `#[votable]`'s hidden edge `table!` must not disturb
//!    `crate::schema::votes`, the `Vote` model, or
//!    `VoteRepository::sum_value_grouped_by_post_id`, which the front-page
//!    "Top posts by votes" leaderboard depends on (including its `IS NOT NULL`
//!    group guard that excludes comment votes). It also seeds a `NULL`-post_id
//!    comment vote *before* reacting, so the whole burst runs against a table
//!    that already holds rows the association must ignore.
//!
//! This suite is illustrative: it is **not** part of CI (the example's
//! Docker tests are not swept). The CI-backed race-safety evidence is
//! `autumn/tests/integration/model_votable.rs`.
//!
//! The Docker-backed tests are `#[ignore]` (like the other PG integration
//! tests) and run via testcontainers by default. When Docker is unavailable,
//! point them at an already-running Postgres instead:
//!
//! ```text
//! AUTUMN_TEST_PG_URL=postgres://autumn:autumn@127.0.0.1:5432/reddit \
//!   cargo test -p reddit-clone --test votable_pg_integration -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` matters only for the external-URL path: each test resets
//! the reddit-clone tables on the shared external database before seeding, so
//! parallel test functions would race on the same tables. A fresh
//! testcontainers Postgres has no such constraint (each test gets its own
//! throwaway container) and runs fine with the default parallel runner.

use autumn_web::repository::ReactionOutcome;
use diesel::sql_types::BigInt;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection};

// The trait `#[votable(by = User, aggregate = sum)]` emits on `Post`, bringing
// `react` / `reaction_of` into scope for `PgPostRepository`.
use reddit_clone::models::PostReactions as _;
use reddit_clone::repositories::{PgPostRepository, PgVoteRepository};
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

const CREATE_SCHEMA: &str = include_str!("../migrations/20260419000000_create_reddit/up.sql");
const ADD_AVATAR: &str = include_str!("../migrations/20260427000000_add_user_avatar/up.sql");
// Comments became polymorphic in #1367; `Subreddit::as_select()` and any
// insert into `comments` need the reshaped table.
const POLYMORPHIC_COMMENTS: &str =
    include_str!("../migrations/20260820000000_polymorphic_comments/up.sql");

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

/// Keeps whichever backing Postgres alive for the duration of the test: a
/// throwaway testcontainers container, or nothing (an already-running external
/// instance pointed to via `AUTUMN_TEST_PG_URL`).
enum PgHandle {
    Container(#[allow(dead_code)] Box<ContainerAsync<Postgres>>),
    External,
}

async fn start_postgres() -> (PgHandle, Pool<AsyncPgConnection>) {
    // Sized to comfortably exceed the 50 concurrent clicks in the race test, so
    // no caller ever queues on pool acquisition. (The *application* runs the
    // default pool of 10 — see `routes::posts` — but a concurrency test
    // deliberately does not.)
    if let Ok(url) = std::env::var("AUTUMN_TEST_PG_URL") {
        let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(url);
        let pool = Pool::builder(manager)
            .max_size(60)
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
        .max_size(60)
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

/// Seed `n` users, one subreddit, and one post per title. Returns the user ids.
async fn seed_users(conn: &mut AsyncPgConnection, n: usize) -> Vec<i64> {
    #[derive(diesel::QueryableByName)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let id = diesel::sql_query(
            "INSERT INTO users (username, password_hash) VALUES ($1, 'h') RETURNING id",
        )
        .bind::<diesel::sql_types::Text, _>(format!("user{i}"))
        .get_result::<IdRow>(conn)
        .await
        .expect("seed user")
        .id;
        ids.push(id);
    }
    ids
}

async fn seed_subreddit(conn: &mut AsyncPgConnection, creator_id: i64) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
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

async fn seed_post(
    conn: &mut AsyncPgConnection,
    slug: &str,
    author_id: i64,
    subreddit_id: i64,
) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct IdRow {
        #[diesel(sql_type = BigInt)]
        id: i64,
    }
    diesel::sql_query(
        "INSERT INTO posts (title, slug, body, author_id, subreddit_id) \
         VALUES ($1, $1, 'body', $2, $3) RETURNING id",
    )
    .bind::<diesel::sql_types::Text, _>(slug)
    .bind::<BigInt, _>(author_id)
    .bind::<BigInt, _>(subreddit_id)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed post")
    .id
}

/// `posts.score` and `SUM(votes.value)` read in **one** statement, so both
/// come from the same snapshot — a mismatch is a real atomicity violation.
#[derive(diesel::QueryableByName)]
struct SnapshotRow {
    #[diesel(sql_type = BigInt)]
    persisted: i64,
    #[diesel(sql_type = BigInt)]
    ground_truth: i64,
}

async fn score_snapshot(conn: &mut AsyncPgConnection, post_id: i64) -> SnapshotRow {
    diesel::sql_query(
        "SELECT p.score AS persisted, \
         COALESCE((SELECT SUM(v.value) FROM votes v WHERE v.post_id = p.id), 0)::BIGINT \
           AS ground_truth \
         FROM posts p WHERE p.id = $1",
    )
    .bind::<BigInt, _>(post_id)
    .get_result::<SnapshotRow>(conn)
    .await
    .expect("read score snapshot")
}

async fn vote_edge_count(conn: &mut AsyncPgConnection, user_id: i64, post_id: i64) -> i64 {
    diesel::sql_query(
        "SELECT COUNT(*)::BIGINT AS count FROM votes WHERE user_id = $1 AND post_id = $2",
    )
    .bind::<BigInt, _>(user_id)
    .bind::<BigInt, _>(post_id)
    .get_result::<CountRow>(conn)
    .await
    .expect("count vote edges")
    .count
}

// ── AC2 + AC3 ─────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn votable_react_upvotes_and_maintains_posts_score() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    let users = seed_users(&mut conn, 2).await;
    let sub = seed_subreddit(&mut conn, users[0]).await;
    let post = seed_post(&mut conn, "hello", users[0], sub).await;

    let repo = PgPostRepository::with_pool_untracked(pool.clone());

    let first = repo.react(users[0], post, 1).await.expect("upvote");
    assert_eq!(first.outcome, ReactionOutcome::Inserted);
    assert_eq!(first.value, Some(1));
    assert_eq!(first.aggregate, 1);

    let second = repo.react(users[1], post, 1).await.expect("second upvote");
    assert_eq!(second.aggregate, 2, "a second reactor adds to the score");

    let snap = score_snapshot(&mut conn, post).await;
    assert_eq!(snap.persisted, 2, "posts.score is persisted, not derived");
    assert_eq!(
        snap.persisted, snap.ground_truth,
        "posts.score must equal SUM(votes.value)"
    );

    // AC4: the reactor's own state is readable for the view layer.
    assert_eq!(
        repo.reaction_of(users[0], post).await.expect("reaction_of"),
        Some(1)
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn votable_react_toggles_off() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    let users = seed_users(&mut conn, 1).await;
    let sub = seed_subreddit(&mut conn, users[0]).await;
    let post = seed_post(&mut conn, "hello", users[0], sub).await;

    let repo = PgPostRepository::with_pool_untracked(pool.clone());
    repo.react(users[0], post, 1).await.expect("upvote");
    let off = repo.react(users[0], post, 1).await.expect("toggle off");

    assert_eq!(off.outcome, ReactionOutcome::Removed);
    assert_eq!(off.value, None);
    assert_eq!(off.aggregate, 0);
    assert_eq!(vote_edge_count(&mut conn, users[0], post).await, 0);
    assert_eq!(
        repo.reaction_of(users[0], post).await.expect("reaction_of"),
        None
    );

    let snap = score_snapshot(&mut conn, post).await;
    assert_eq!(snap.persisted, 0);
    assert_eq!(snap.persisted, snap.ground_truth);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn votable_react_flips() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    let users = seed_users(&mut conn, 1).await;
    let sub = seed_subreddit(&mut conn, users[0]).await;
    let post = seed_post(&mut conn, "hello", users[0], sub).await;

    let repo = PgPostRepository::with_pool_untracked(pool.clone());
    repo.react(users[0], post, 1).await.expect("upvote");
    let flipped = repo.react(users[0], post, -1).await.expect("downvote");

    assert_eq!(flipped.outcome, ReactionOutcome::Flipped);
    assert_eq!(flipped.value, Some(-1));
    assert_eq!(flipped.aggregate, -1);
    assert_eq!(
        vote_edge_count(&mut conn, users[0], post).await,
        1,
        "a flip replaces the shipped table's row, honouring votes_unique_post"
    );

    let snap = score_snapshot(&mut conn, post).await;
    assert_eq!(snap.persisted, -1);
    assert_eq!(snap.persisted, snap.ground_truth);
}

/// The issue's headline success metric, on the real table: 50 simultaneous
/// clicks from one user on one post. No caller may see a `23505`, and because
/// the target-row lock serialises the whole read-decide-write window, 50
/// concurrent same-value clicks are 50 sequential toggles — an even count, so
/// the pair must end with **no** edge and `posts.score` back at `0`. That holds
/// under every interleaving, so the test cannot flake on scheduling. (The
/// framework suite pins the odd-count case too.)
#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn votable_react_is_race_safe_on_the_shipped_votes_table() {
    const CLICKS: usize = 50;

    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    let users = seed_users(&mut conn, 1).await;
    let sub = seed_subreddit(&mut conn, users[0]).await;
    let post = seed_post(&mut conn, "contended", users[0], sub).await;

    let repo = PgPostRepository::with_pool_untracked(pool.clone());
    let mut handles = Vec::with_capacity(CLICKS);
    for _ in 0..CLICKS {
        let repo = repo.clone();
        let user = users[0];
        handles.push(tokio::spawn(async move { repo.react(user, post, 1).await }));
    }
    for h in handles {
        h.await
            .expect("task did not panic")
            .expect("no unique-violation (23505) may surface to any caller");
    }

    let edges = vote_edge_count(&mut conn, users[0], post).await;
    assert_eq!(
        edges, 0,
        "the target-row lock serialises the clicks, so 50 same-value toggles \
         from empty must end at exactly 0 edges, got {edges}"
    );

    let snap = score_snapshot(&mut conn, post).await;
    assert_eq!(
        snap.persisted, 0,
        "an even number of toggles leaves posts.score at 0"
    );
    assert_eq!(
        snap.persisted, snap.ground_truth,
        "posts.score ({}) must equal SUM(votes.value) ({}) after 50 concurrent clicks",
        snap.persisted, snap.ground_truth
    );
}

// ── Regression guard: the #1364 leaderboard is untouched ──────────────────────

/// `#[votable]` declares its edge table inside a private hidden module, so
/// `crate::schema::votes`, the `Vote` model and
/// `VoteRepository::sum_value_grouped_by_post_id` must all keep working
/// unchanged — including the nullable-`post_id` `IS NOT NULL` group guard that
/// excludes comment votes from the front-page leaderboard.
#[tokio::test]
#[ignore = "requires Docker (testcontainers) or AUTUMN_TEST_PG_URL"]
async fn leaderboard_grouped_aggregate_still_works_after_react() {
    let (_c, pool) = start_postgres().await;
    let mut conn = pool.get().await.expect("conn");
    setup_schema(&mut conn).await;
    let users = seed_users(&mut conn, 4).await;
    let sub = seed_subreddit(&mut conn, users[0]).await;
    let hot = seed_post(&mut conn, "hot", users[0], sub).await;
    let cold = seed_post(&mut conn, "cold", users[0], sub).await;

    // Seed the comment vote FIRST, so every `react()` below runs against a
    // `votes` table that already contains rows with a `NULL` post_id. That is
    // the direction that matters: the generated code declares `post_id`
    // non-nullable in its hidden `table!` and recomputes with
    // `WHERE post_id = $t`, so a pre-existing NULL row must neither be summed
    // into a post's score nor collide with `votes_unique_post` (a Postgres
    // unique constraint treats NULLs as distinct). The same rows also prove the
    // leaderboard's `IS NOT NULL` group guard still excludes comment votes.
    diesel::sql_query(
        "INSERT INTO comments (commentable_type, commentable_id, author_id, body) \
         VALUES ('Post', $1, $2, 'c')",
    )
        .bind::<BigInt, _>(hot)
        .bind::<BigInt, _>(users[0])
        .execute(&mut conn)
        .await
        .expect("seed comment");
    diesel::sql_query(
        "INSERT INTO votes (user_id, comment_id, value) \
         VALUES ($1, (SELECT id FROM comments LIMIT 1), 1)",
    )
    .bind::<BigInt, _>(users[0])
    .execute(&mut conn)
    .await
    .expect("seed comment vote");

    let posts = PgPostRepository::with_pool_untracked(pool.clone());
    // A react burst: `hot` ends on +3, `cold` on -1. `users[0]` already holds a
    // NULL-post_id vote row, and still reacts to `hot` normally.
    for user in &users[..3] {
        posts.react(*user, hot, 1).await.expect("upvote hot");
    }
    posts
        .react(users[3], cold, -1)
        .await
        .expect("downvote cold");

    // The scores the votable path persisted — the comment vote is not in them.
    assert_eq!(score_snapshot(&mut conn, hot).await.persisted, 3);
    assert_eq!(score_snapshot(&mut conn, cold).await.persisted, -1);

    let votes_repo = PgVoteRepository::with_pool_untracked(pool.clone());
    let top: Vec<(i64, Option<i64>)> = votes_repo
        .sum_value_grouped_by_post_id()
        .order_by_aggregate_desc()
        .limit(5)
        .load()
        .await
        .expect("grouped aggregate still works after react()");

    assert_eq!(
        top.len(),
        2,
        "one row per post-directed vote group; the comment vote's NULL group is \
         excluded: {top:?}"
    );
    assert_eq!(top[0], (hot, Some(3)), "hot leads the leaderboard: {top:?}");
    assert_eq!(top[1], (cold, Some(-1)), "cold trails: {top:?}");
}
