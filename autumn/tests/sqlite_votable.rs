//! `#[votable]` reaction helpers on the `SQLite` runtime backend (issue #1362).
//!
//! The Postgres behaviour suite (`tests/integration/model_votable.rs`) needs
//! Docker and is `#[ignore]`d; nothing there ever compiles — let alone runs —
//! the `sqlite` arm of the generated `react()`. Yet the guide, the ADR and the
//! generated rustdoc all claim `SQLite` is supported: the `backend_select!`
//! `sqlite` arm drops the Postgres row lock because `BEGIN IMMEDIATE` (which
//! `scoped_immediate_transaction` takes) already gives strictly stronger
//! single-writer mutual exclusion. This file is the CI-backed evidence for that
//! claim — it is the only place the `sqlite` arm is type-checked at all.
//!
//! What it pins:
//!
//! * **Compile proof.** `react` / `reaction_of` monomorphize on the `SQLite`
//!   backend in both aggregate modes — the `sum(Int2)` aggregate, the
//!   `ON CONFLICT … DO UPDATE SET value = excluded.value` upsert, the
//!   `COUNT(*)` aggregate and the `deleted_at IS NULL` soft-delete gate all
//!   have to resolve against `SqliteConnection`, not just `AsyncPgConnection`.
//! * **Toggle / re-insert.** insert → toggle-off → re-insert leaves the
//!   aggregate at `+1 / 0 / +1` and never more than one edge row.
//! * **Flip.** `+1` then `-1` replaces the value in place (one edge row,
//!   aggregate `-1`, outcome `Flipped`).
//! * **Soft delete (AC6).** Reacting to a soft-deleted target is `NotFound`,
//!   creates no edge and leaves the aggregate untouched — the `sqlite` arm's S1
//!   select carries the same `deleted_at IS NULL` guard as the `pg` arm.
//! * **Count mode.** Unary membership toggles and `like_count` tracks
//!   `COUNT(*)`.
//!
//! Uses an in-memory shared-cache `SQLite` database — no Docker.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_votable`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::repository::ReactionOutcome;
use axum::http::StatusCode;

use diesel::sql_types::BigInt;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

// ── Reactor ───────────────────────────────────────────────────────────────────

mod schema {
    autumn_web::reexports::diesel::table! {
        votable_voters (id) {
            id -> Int8,
            name -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        votable_vote_posts (id) {
            id -> Int8,
            title -> Text,
            score -> Int8,
            deleted_at -> Nullable<Timestamp>,
        }
    }

    autumn_web::reexports::diesel::table! {
        votable_like_posts (id) {
            id -> Int8,
            title -> Text,
            like_count -> Int8,
        }
    }
}

use schema::{votable_like_posts, votable_vote_posts, votable_voters};

#[autumn_web::model(table = "votable_voters")]
pub struct Voter {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(Voter, table = "votable_voters")]
pub trait VoterRepository {}

// ── Sum mode + soft delete ────────────────────────────────────────────────────

#[autumn_web::model(table = "votable_vote_posts")]
#[votable(
    by = Voter,
    aggregate = sum,
    table = votable_post_votes,
    reactor_fk = voter_id,
    target_fk = post_id
)]
pub struct VotePost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub score: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(VotePost, table = "votable_vote_posts", soft_delete)]
pub trait VotePostRepository {}

// ── Count mode (unary likes) ──────────────────────────────────────────────────

#[autumn_web::model(table = "votable_like_posts")]
#[votable(
    by = Voter,
    aggregate = count,
    name = like,
    table = votable_post_likes,
    reactor_fk = voter_id,
    target_fk = post_id
)]
pub struct LikePost {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub like_count: i64,
}

#[autumn_web::repository(LikePost, table = "votable_like_posts")]
pub trait VotePostLikeRepository {}

// ── Setup & helpers ───────────────────────────────────────────────────────────

/// The composite `UNIQUE (voter_id, post_id)` on each edge table is
/// load-bearing: it is the `ON CONFLICT` arbiter the generated upsert names by
/// column list, so a single-column (or missing) constraint would fail the
/// insert arm outright. `votable_post_votes` also carries a surrogate `id` and a
/// `CHECK` on `value`, mirroring the shape a real app ships.
const DDL: &[&str] = &[
    "CREATE TABLE votable_voters (\
         id INTEGER PRIMARY KEY AUTOINCREMENT, \
         name TEXT NOT NULL\
     )",
    "CREATE TABLE votable_vote_posts (\
         id INTEGER PRIMARY KEY AUTOINCREMENT, \
         title TEXT NOT NULL, \
         score BIGINT NOT NULL DEFAULT 0, \
         deleted_at TIMESTAMP\
     )",
    "CREATE TABLE votable_post_votes (\
         id INTEGER PRIMARY KEY AUTOINCREMENT, \
         voter_id BIGINT NOT NULL, \
         post_id BIGINT NOT NULL, \
         value SMALLINT NOT NULL CHECK (value IN (-1, 1)), \
         UNIQUE (voter_id, post_id)\
     )",
    "CREATE TABLE votable_like_posts (\
         id INTEGER PRIMARY KEY AUTOINCREMENT, \
         title TEXT NOT NULL, \
         like_count BIGINT NOT NULL DEFAULT 0\
     )",
    "CREATE TABLE votable_post_likes (\
         voter_id BIGINT NOT NULL, \
         post_id BIGINT NOT NULL, \
         PRIMARY KEY (voter_id, post_id)\
     )",
];

async fn boot_pool(db_name: &str) -> SqlitePool {
    // A shared-cache in-memory database so every pooled checkout observes the
    // same schema (a bare `:memory:` target is private per connection).
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(2),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        for stmt in DDL {
            diesel::sql_query(*stmt)
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|e| panic!("DDL failed ({stmt}): {e}"));
        }
    }

    pool
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

#[derive(diesel::QueryableByName)]
struct ScoreRow {
    #[diesel(sql_type = BigInt)]
    score: i64,
}

/// The persisted aggregate, read with raw SQL so the assertion never depends on
/// the repository's own read path (which the soft-delete tests deliberately
/// take out of play).
async fn post_score(pool: &SqlitePool, id: i64) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("SELECT score FROM votable_vote_posts WHERE id = ?")
        .bind::<BigInt, _>(id)
        .get_result::<ScoreRow>(&mut *conn)
        .await
        .expect("read score")
        .score
}

async fn like_count(pool: &SqlitePool, id: i64) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("SELECT like_count AS count FROM votable_like_posts WHERE id = ?")
        .bind::<BigInt, _>(id)
        .get_result::<CountRow>(&mut *conn)
        .await
        .expect("read like_count")
        .count
}

/// Edge rows for one `(reactor, target)` pair — the "at most one edge" invariant
/// the composite unique constraint enforces.
async fn edge_count(pool: &SqlitePool, table: &str, voter: i64, post: i64) -> i64 {
    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(format!(
        "SELECT COUNT(*) AS count FROM {table} WHERE voter_id = ? AND post_id = ?"
    ))
    .bind::<BigInt, _>(voter)
    .bind::<BigInt, _>(post)
    .get_result::<CountRow>(&mut *conn)
    .await
    .expect("count edges")
    .count
}

async fn seed_voter(repo: &PgVoterRepository, name: &str) -> i64 {
    repo.save(&NewVoter {
        name: name.to_owned(),
    })
    .await
    .expect("seed voter")
    .id
}

// ── Compile proof ─────────────────────────────────────────────────────────────

/// The `sqlite` arm of the generated `react()` / `reaction_of()` type-checks and
/// monomorphizes — the assertion that nothing else in this workspace makes.
///
/// Not `#[ignore]`d and needs no database: naming the associated functions is
/// enough to force the whole `SQLite` call chain (locking-free S1 select, the
/// `ON CONFLICT` upsert, `sum(Int2)` / `COUNT(*)`, the S5 update) through trait
/// resolution against `SqliteConnection`.
#[test]
fn votable_methods_monomorphize_on_sqlite() {
    fn assert_is_fn<F>(_f: F) {}

    assert_is_fn(<PgVotePostRepository as VotePostReactions>::react);
    assert_is_fn(<PgVotePostRepository as VotePostReactions>::reaction_of);
    assert_is_fn(<PgVotePostLikeRepository as LikePostReactions>::react);
    assert_is_fn(<PgVotePostLikeRepository as LikePostReactions>::reaction_of);
}

// ── Behaviour ─────────────────────────────────────────────────────────────────

/// Insert → toggle-off → re-insert. The aggregate follows `SUM(value)` at every
/// step and the edge count never exceeds one, which is the whole contract in
/// sum mode.
#[tokio::test]
async fn react_toggles_off_and_reinserts_on_sqlite() {
    let pool = boot_pool("votable_toggle").await;
    let voters = PgVoterRepository::with_pool_untracked(pool.clone());
    let posts = PgVotePostRepository::with_pool_untracked(pool.clone());
    let ada = seed_voter(&voters, "ada").await;
    let post = posts
        .save(&NewVotePost {
            title: "hello".to_owned(),
        })
        .await
        .expect("seed post")
        .id;

    let first = posts.react(ada, post, 1).await.expect("first upvote");
    assert_eq!(first.outcome, ReactionOutcome::Inserted);
    assert_eq!(first.value, Some(1));
    assert_eq!(first.aggregate, 1);
    assert_eq!(post_score(&pool, post).await, 1);
    assert_eq!(edge_count(&pool, "votable_post_votes", ada, post).await, 1);

    // The same value again is a toggle-off, not a second edge.
    let second = posts.react(ada, post, 1).await.expect("toggle off");
    assert_eq!(second.outcome, ReactionOutcome::Removed);
    assert_eq!(second.value, None);
    assert_eq!(second.aggregate, 0);
    assert_eq!(post_score(&pool, post).await, 0);
    assert_eq!(edge_count(&pool, "votable_post_votes", ada, post).await, 0);
    assert_eq!(
        posts.reaction_of(ada, post).await.expect("reaction_of"),
        None
    );

    // Re-inserting after a toggle-off goes through the upsert arm again.
    let third = posts.react(ada, post, 1).await.expect("re-insert");
    assert_eq!(third.outcome, ReactionOutcome::Inserted);
    assert_eq!(third.aggregate, 1);
    assert_eq!(post_score(&pool, post).await, 1);
    assert_eq!(edge_count(&pool, "votable_post_votes", ada, post).await, 1);
    assert_eq!(
        posts.reaction_of(ada, post).await.expect("reaction_of"),
        Some(1)
    );
}

/// A flip replaces the value *in place*: one edge row throughout, and the
/// aggregate swings by two rather than accumulating a second vote.
#[tokio::test]
async fn react_flips_the_edge_in_place_on_sqlite() {
    let pool = boot_pool("votable_flip").await;
    let voters = PgVoterRepository::with_pool_untracked(pool.clone());
    let posts = PgVotePostRepository::with_pool_untracked(pool.clone());
    let ada = seed_voter(&voters, "ada").await;
    let bob = seed_voter(&voters, "bob").await;
    let post = posts
        .save(&NewVotePost {
            title: "flip".to_owned(),
        })
        .await
        .expect("seed post")
        .id;

    posts.react(ada, post, 1).await.expect("upvote");
    posts.react(bob, post, 1).await.expect("second upvote");
    assert_eq!(post_score(&pool, post).await, 2);

    let flipped = posts.react(ada, post, -1).await.expect("flip to downvote");
    assert_eq!(flipped.outcome, ReactionOutcome::Flipped);
    assert_eq!(flipped.value, Some(-1));
    assert_eq!(flipped.aggregate, 0, "+1 (bob) + -1 (ada)");
    assert_eq!(post_score(&pool, post).await, 0);
    assert_eq!(
        edge_count(&pool, "votable_post_votes", ada, post).await,
        1,
        "a flip must never create a second edge row"
    );
    assert_eq!(
        posts.reaction_of(ada, post).await.expect("reaction_of"),
        Some(-1)
    );
}

/// AC6 on `SQLite`: the `sqlite` arm's S1 select carries the same
/// `deleted_at IS NULL` guard as the `pg` arm, so a soft-deleted target is
/// `NotFound`, gains no edge and keeps its aggregate.
#[tokio::test]
async fn react_on_a_soft_deleted_target_is_not_found_on_sqlite() {
    let pool = boot_pool("votable_soft_delete").await;
    let voters = PgVoterRepository::with_pool_untracked(pool.clone());
    let posts = PgVotePostRepository::with_pool_untracked(pool.clone());
    let ada = seed_voter(&voters, "ada").await;
    let bob = seed_voter(&voters, "bob").await;
    let post = posts
        .save(&NewVotePost {
            title: "doomed".to_owned(),
        })
        .await
        .expect("seed post")
        .id;

    posts.react(ada, post, 1).await.expect("live target reacts");
    assert_eq!(post_score(&pool, post).await, 1);

    // `soft_delete` on the repository stamps `deleted_at` rather than removing
    // the row — exactly the state the votable guard has to notice.
    posts.delete_by_id(post).await.expect("soft delete");

    let err = posts
        .react(bob, post, 1)
        .await
        .expect_err("a soft-deleted target must not accept reactions");
    assert_eq!(err.status(), StatusCode::NOT_FOUND);

    assert_eq!(
        post_score(&pool, post).await,
        1,
        "the aggregate of a soft-deleted target is untouched"
    );
    assert_eq!(
        edge_count(&pool, "votable_post_votes", bob, post).await,
        0,
        "no edge is created against a soft-deleted target"
    );
}

/// Reacting to a target that never existed is `NotFound`, not a foreign-key
/// error surfaced as a 500.
#[tokio::test]
async fn react_on_a_missing_target_is_not_found_on_sqlite() {
    let pool = boot_pool("votable_missing").await;
    let voters = PgVoterRepository::with_pool_untracked(pool.clone());
    let posts = PgVotePostRepository::with_pool_untracked(pool.clone());
    let ada = seed_voter(&voters, "ada").await;

    let err = posts
        .react(ada, 987_654, 1)
        .await
        .expect_err("missing target must be NotFound");
    assert_eq!(err.status(), StatusCode::NOT_FOUND);
}

/// Count mode on `SQLite`: `react()` takes no value, a repeat click can only
/// toggle membership off, and the aggregate tracks `COUNT(*)`.
#[tokio::test]
async fn count_mode_react_toggles_membership_on_sqlite() {
    let pool = boot_pool("votable_count").await;
    let voters = PgVoterRepository::with_pool_untracked(pool.clone());
    let posts = PgVotePostLikeRepository::with_pool_untracked(pool.clone());
    let ada = seed_voter(&voters, "ada").await;
    let bob = seed_voter(&voters, "bob").await;
    let post = posts
        .save(&NewLikePost {
            title: "likeable".to_owned(),
        })
        .await
        .expect("seed post")
        .id;

    let first = posts.react(ada, post).await.expect("like");
    assert_eq!(first.outcome, ReactionOutcome::Inserted);
    assert_eq!(
        first.value,
        Some(1),
        "count mode reports Some(1) while the membership row exists"
    );
    assert_eq!(first.aggregate, 1);

    posts.react(bob, post).await.expect("second like");
    assert_eq!(like_count(&pool, post).await, 2);

    let unliked = posts.react(ada, post).await.expect("unlike");
    assert_eq!(unliked.outcome, ReactionOutcome::Removed);
    assert_eq!(unliked.value, None);
    assert_eq!(unliked.aggregate, 1);
    assert_eq!(like_count(&pool, post).await, 1);
    assert_eq!(edge_count(&pool, "votable_post_likes", ada, post).await, 0);
    assert_eq!(
        posts.reaction_of(ada, post).await.expect("reaction_of"),
        None
    );
    assert_eq!(
        posts.reaction_of(bob, post).await.expect("reaction_of"),
        Some(1)
    );
}
