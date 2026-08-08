//! Database-level integration tests for the declarative `#[votable]` reaction
//! association (issue #1362).
//!
//! The behavioural tests **require Docker** (testcontainers) and are
//! `#[ignore]`d by default; CI's ignored-test sweep runs them. One non-ignored
//! test proves the generated `react` / `reaction_of` surface exists and
//! monomorphizes without a live database (the #1592 guard pattern).
//!
//! Fixtures span every generated branch:
//!
//! | Fixture | Attribute | Edge table | Aggregate column |
//! |---|---|---|---|
//! | `VotableTarget` | `#[votable(by = VotableReactor, aggregate = sum)]` | `votes` (pure default) | `score` (default) |
//! | `VotableSoftTarget` | `… aggregate = sum, table = votable_soft_votes` | `votable_soft_votes` | `score` (default) |
//! | `VotableLikeTarget` | `… aggregate = count, name = like` | `likes` | `like_count` |
//! | `VotableSoftLikeTarget` | `… aggregate = count, name = soft_like` | `soft_likes` | `soft_like_count` |
//! | `VotableXorTarget` | `… aggregate = sum, table = votable_xor_votes` | `votable_xor_votes` (nullable target FK) | `score` (default) |
//!
//! `VotableTarget` deliberately exercises **pure defaults** — `table` resolves
//! to `pluralize("vote") == "votes"`, `reactor_fk` to `votable_reactor_id`,
//! `target_fk` to `votable_target_id`, `value_column` to `value` and `column`
//! to `score`. Every other fixture must override `table` (or `name`, which
//! drives it) because several votable models in one test binary would
//! otherwise all resolve onto the same `votes` edge table.
//!
//! The composite `UNIQUE (reactor_fk, target_fk)` on every edge table is
//! load-bearing: it is the `ON CONFLICT` arbiter the generated upsert names,
//! and it is what makes "at most one edge per (reactor, target)" a database
//! guarantee rather than an application convention.
//!
//! `VotableXorTarget` deliberately pins the *awkward* real-world shape
//! reddit-clone ships: the edge table's target FK is **nullable** in the DDL
//! (its `votes` table is an XOR over `post_id` / `comment_id`) while the hidden
//! generated `table!` declares it non-nullable, and rows with a `NULL` target
//! are already present before any `react()` runs. That coexistence is a
//! documented tolerance in `docs/guide/votable.md`, so it is pinned here rather
//! than only in the example's (non-CI) suite.
//!
//! Concurrency assertions are **invariant-based**, never timing-based, and
//! they are *exact* rather than bounded: because the target-row lock serialises
//! the whole read-decide-write window, N concurrent same-value clicks are N
//! sequential toggles, so the final edge count and aggregate are determined by
//! N's parity alone — they hold under *every* interleaving, and the tests
//! cannot flake on scheduling.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use autumn_web::repository::{Reaction, ReactionOutcome};
use axum::http::StatusCode;
use diesel::sql_types::BigInt;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Reactor ───────────────────────────────────────────────────────────────────

diesel::table! {
    votable_reactors (id) {
        id -> Int8,
        name -> Text,
    }
}

#[autumn_web::model(table = "votable_reactors")]
pub struct VotableReactor {
    #[id]
    pub id: i64,
    pub name: String,
}

// ── Sum mode, pure defaults (edge table `votes`, aggregate column `score`) ────

diesel::table! {
    votable_targets (id) {
        id -> Int8,
        title -> Text,
        score -> Int8,
    }
}

#[autumn_web::model(table = "votable_targets")]
#[votable(by = VotableReactor, aggregate = sum)]
pub struct VotableTarget {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub score: i64,
}

#[autumn_web::repository(VotableTarget, table = "votable_targets")]
pub trait VotableTargetRepository {
    /// Reads a target back through the repository layer (not raw SQL) so the
    /// persisted aggregate can be asserted the way an application would see it.
    fn find_by_title(title: String) -> Vec<VotableTarget>;
}

// ── Sum mode + soft delete (AC6) ──────────────────────────────────────────────

diesel::table! {
    votable_soft_targets (id) {
        id -> Int8,
        title -> Text,
        score -> Int8,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "votable_soft_targets")]
#[votable(by = VotableReactor, aggregate = sum, table = votable_soft_votes)]
pub struct VotableSoftTarget {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub score: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(VotableSoftTarget, table = "votable_soft_targets", soft_delete)]
pub trait VotableSoftTargetRepository {
    fn find_by_title(title: String) -> Vec<VotableSoftTarget>;
}

// ── Count mode (unary likes) ──────────────────────────────────────────────────

diesel::table! {
    votable_like_targets (id) {
        id -> Int8,
        title -> Text,
        like_count -> Int8,
    }
}

#[autumn_web::model(table = "votable_like_targets")]
#[votable(by = VotableReactor, aggregate = count, name = like)]
pub struct VotableLikeTarget {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub like_count: i64,
}

#[autumn_web::repository(VotableLikeTarget, table = "votable_like_targets")]
pub trait VotableLikeTargetRepository {
    fn find_by_title(title: String) -> Vec<VotableLikeTarget>;
}

// ── Count mode + soft delete ──────────────────────────────────────────────────

diesel::table! {
    votable_soft_like_targets (id) {
        id -> Int8,
        title -> Text,
        soft_like_count -> Int8,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "votable_soft_like_targets")]
#[votable(by = VotableReactor, aggregate = count, name = soft_like)]
pub struct VotableSoftLikeTarget {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub soft_like_count: i64,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    VotableSoftLikeTarget,
    table = "votable_soft_like_targets",
    soft_delete
)]
pub trait VotableSoftLikeTargetRepository {
    fn find_by_title(title: String) -> Vec<VotableSoftLikeTarget>;
}

// ── Sum mode over a nullable target FK (reddit-clone's XOR shape) ─────────────

diesel::table! {
    votable_xor_targets (id) {
        id -> Int8,
        title -> Text,
        score -> Int8,
    }
}

#[autumn_web::model(table = "votable_xor_targets")]
#[votable(by = VotableReactor, aggregate = sum, table = votable_xor_votes)]
pub struct VotableXorTarget {
    #[id]
    pub id: i64,
    pub title: String,
    #[default]
    pub score: i64,
}

#[autumn_web::repository(VotableXorTarget, table = "votable_xor_targets")]
pub trait VotableXorTargetRepository {
    fn find_by_title(title: String) -> Vec<VotableXorTarget>;
}

// ── Setup & helpers ───────────────────────────────────────────────────────────

/// Every DDL statement the fixtures need. The edge tables differ on purpose:
///
/// - `votes` carries a surrogate `id` and a `created_at`, exactly like
///   reddit-clone's shipped table, proving the generated code tolerates extra
///   columns it never names.
/// - `votable_soft_votes`, `likes` and `soft_likes` are the minimal shape (the
///   composite key only), proving no surrogate key is required.
/// - `votable_xor_votes` is reddit-clone's shape: a surrogate `id`, a
///   **nullable** target FK, a second nullable FK to a different target kind, an
///   XOR check across the two, and one composite `UNIQUE` per kind. The
///   generated code declares the target FK non-nullable and never writes the
///   sibling column, so the two coexist.
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS votable_reactors \
     (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS votable_targets \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, score BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS votable_soft_targets \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, score BIGINT NOT NULL DEFAULT 0, \
      deleted_at TIMESTAMP NULL)",
    "CREATE TABLE IF NOT EXISTS votable_like_targets \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, like_count BIGINT NOT NULL DEFAULT 0)",
    "CREATE TABLE IF NOT EXISTS votes \
     (id BIGSERIAL PRIMARY KEY, \
      votable_reactor_id BIGINT NOT NULL REFERENCES votable_reactors(id), \
      votable_target_id BIGINT NOT NULL REFERENCES votable_targets(id) ON DELETE CASCADE, \
      value SMALLINT NOT NULL CHECK (value IN (-1, 1)), \
      created_at TIMESTAMP NOT NULL DEFAULT NOW(), \
      CONSTRAINT votes_unique_pair UNIQUE (votable_reactor_id, votable_target_id))",
    "CREATE TABLE IF NOT EXISTS votable_soft_votes \
     (votable_reactor_id BIGINT NOT NULL REFERENCES votable_reactors(id), \
      votable_soft_target_id BIGINT NOT NULL \
        REFERENCES votable_soft_targets(id) ON DELETE CASCADE, \
      value SMALLINT NOT NULL CHECK (value IN (-1, 1)), \
      CONSTRAINT votable_soft_votes_unique_pair \
        UNIQUE (votable_reactor_id, votable_soft_target_id))",
    "CREATE TABLE IF NOT EXISTS likes \
     (votable_reactor_id BIGINT NOT NULL REFERENCES votable_reactors(id), \
      votable_like_target_id BIGINT NOT NULL \
        REFERENCES votable_like_targets(id) ON DELETE CASCADE, \
      CONSTRAINT likes_unique_pair UNIQUE (votable_reactor_id, votable_like_target_id))",
    "CREATE TABLE IF NOT EXISTS votable_soft_like_targets \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, \
      soft_like_count BIGINT NOT NULL DEFAULT 0, deleted_at TIMESTAMP NULL)",
    "CREATE TABLE IF NOT EXISTS soft_likes \
     (votable_reactor_id BIGINT NOT NULL REFERENCES votable_reactors(id), \
      votable_soft_like_target_id BIGINT NOT NULL \
        REFERENCES votable_soft_like_targets(id) ON DELETE CASCADE, \
      CONSTRAINT soft_likes_unique_pair \
        UNIQUE (votable_reactor_id, votable_soft_like_target_id))",
    "CREATE TABLE IF NOT EXISTS votable_xor_targets \
     (id BIGSERIAL PRIMARY KEY, title TEXT NOT NULL, score BIGINT NOT NULL DEFAULT 0)",
    // The XOR sibling target kind (reddit-clone's `comments`).
    "CREATE TABLE IF NOT EXISTS votable_xor_others (id BIGSERIAL PRIMARY KEY)",
    "CREATE TABLE IF NOT EXISTS votable_xor_votes \
     (id BIGSERIAL PRIMARY KEY, \
      votable_reactor_id BIGINT NOT NULL REFERENCES votable_reactors(id), \
      votable_xor_target_id BIGINT REFERENCES votable_xor_targets(id) ON DELETE CASCADE, \
      votable_xor_other_id BIGINT REFERENCES votable_xor_others(id) ON DELETE CASCADE, \
      value SMALLINT NOT NULL CHECK (value IN (-1, 1)), \
      CONSTRAINT votable_xor_votes_target_check CHECK ( \
        (votable_xor_target_id IS NOT NULL AND votable_xor_other_id IS NULL) OR \
        (votable_xor_target_id IS NULL AND votable_xor_other_id IS NOT NULL)), \
      CONSTRAINT votable_xor_votes_unique_target \
        UNIQUE (votable_reactor_id, votable_xor_target_id), \
      CONSTRAINT votable_xor_votes_unique_other \
        UNIQUE (votable_reactor_id, votable_xor_other_id))",
];

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    // Sized to comfortably exceed the 50 concurrent race callers below plus the
    // observer connection, so no caller ever queues on pool acquisition.
    let pool = Pool::builder(manager).max_size(60).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    for stmt in DDL {
        diesel::sql_query(*stmt)
            .execute(&mut conn)
            .await
            .unwrap_or_else(|e| panic!("DDL failed ({stmt}): {e}"));
    }
    drop(conn);

    (pool, container)
}

#[derive(diesel::QueryableByName)]
struct IdRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

#[derive(diesel::QueryableByName)]
struct CountRow {
    #[diesel(sql_type = BigInt)]
    count: i64,
}

/// A `(persisted aggregate, ground truth)` pair read in **one** SQL statement,
/// so both values necessarily come from the same snapshot. This is what makes
/// the reader-observation test meaningful.
#[derive(diesel::QueryableByName)]
struct SnapshotRow {
    #[diesel(sql_type = BigInt)]
    persisted: i64,
    #[diesel(sql_type = BigInt)]
    ground_truth: i64,
}

async fn seed_reactor(conn: &mut AsyncPgConnection, name: &str) -> i64 {
    diesel::sql_query("INSERT INTO votable_reactors (name) VALUES ($1) RETURNING id")
        .bind::<diesel::sql_types::Text, _>(name)
        .get_result::<IdRow>(conn)
        .await
        .expect("seed reactor")
        .id
}

async fn seed_reactors(conn: &mut AsyncPgConnection, n: usize) -> Vec<i64> {
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        ids.push(seed_reactor(conn, &format!("reactor-{i}")).await);
    }
    ids
}

async fn seed_target(conn: &mut AsyncPgConnection, table: &str, title: &str) -> i64 {
    diesel::sql_query(format!(
        "INSERT INTO {table} (title) VALUES ($1) RETURNING id"
    ))
    .bind::<diesel::sql_types::Text, _>(title)
    .get_result::<IdRow>(conn)
    .await
    .expect("seed target")
    .id
}

/// `votable_targets.score` alongside `SUM(votes.value)`, in one statement.
async fn sum_snapshot(conn: &mut AsyncPgConnection, target_id: i64) -> SnapshotRow {
    diesel::sql_query(
        "SELECT t.score AS persisted, \
         COALESCE((SELECT SUM(v.value) FROM votes v \
                   WHERE v.votable_target_id = t.id), 0)::BIGINT AS ground_truth \
         FROM votable_targets t WHERE t.id = $1",
    )
    .bind::<BigInt, _>(target_id)
    .get_result::<SnapshotRow>(conn)
    .await
    .expect("read sum snapshot")
}

/// `votable_like_targets.like_count` alongside `COUNT(likes.*)`, in one
/// statement.
async fn count_snapshot(conn: &mut AsyncPgConnection, target_id: i64) -> SnapshotRow {
    diesel::sql_query(
        "SELECT t.like_count AS persisted, \
         (SELECT COUNT(*) FROM likes l \
          WHERE l.votable_like_target_id = t.id)::BIGINT AS ground_truth \
         FROM votable_like_targets t WHERE t.id = $1",
    )
    .bind::<BigInt, _>(target_id)
    .get_result::<SnapshotRow>(conn)
    .await
    .expect("read count snapshot")
}

/// `votable_xor_targets.score` alongside `SUM(value)` over the **non-`NULL`**
/// target rows of the XOR edge table, in one statement. Rows whose target FK is
/// `NULL` (the sibling target kind) must not contribute.
async fn xor_snapshot(conn: &mut AsyncPgConnection, target_id: i64) -> SnapshotRow {
    diesel::sql_query(
        "SELECT t.score AS persisted, \
         COALESCE((SELECT SUM(v.value) FROM votable_xor_votes v \
                   WHERE v.votable_xor_target_id = t.id), 0)::BIGINT AS ground_truth \
         FROM votable_xor_targets t WHERE t.id = $1",
    )
    .bind::<BigInt, _>(target_id)
    .get_result::<SnapshotRow>(conn)
    .await
    .expect("read xor snapshot")
}

/// `votable_soft_like_targets.soft_like_count`, read directly.
async fn soft_like_count(conn: &mut AsyncPgConnection, target_id: i64) -> i64 {
    diesel::sql_query(
        "SELECT soft_like_count AS count FROM votable_soft_like_targets WHERE id = $1",
    )
    .bind::<BigInt, _>(target_id)
    .get_result::<CountRow>(conn)
    .await
    .expect("read soft like count")
    .count
}

/// `votable_soft_targets.score`, read directly (the soft-delete tests assert it
/// is left untouched).
async fn soft_target_score(conn: &mut AsyncPgConnection, target_id: i64) -> i64 {
    diesel::sql_query("SELECT score AS count FROM votable_soft_targets WHERE id = $1")
        .bind::<BigInt, _>(target_id)
        .get_result::<CountRow>(conn)
        .await
        .expect("read soft target score")
        .count
}

async fn edge_count(
    conn: &mut AsyncPgConnection,
    table: &str,
    target_fk: &str,
    reactor_id: i64,
    target_id: i64,
) -> i64 {
    diesel::sql_query(format!(
        "SELECT COUNT(*)::BIGINT AS count FROM {table} \
         WHERE votable_reactor_id = $1 AND {target_fk} = $2"
    ))
    .bind::<BigInt, _>(reactor_id)
    .bind::<BigInt, _>(target_id)
    .get_result::<CountRow>(conn)
    .await
    .expect("count edges")
    .count
}

/// Deterministic ±1 sequence for the multi-reactor burst.
///
/// Index-derived rather than random, so a failing interleaving is reproducible
/// and the test can never flake on a seed. The stride is coprime with the table
/// length so reactors do not march in lockstep — the burst therefore mixes
/// inserts, flips and toggle-offs.
const BURST_VALUES: [i16; 7] = [1, 1, -1, 1, -1, -1, 1];

fn burst_value(reactor_index: usize, round: usize) -> i16 {
    BURST_VALUES[(reactor_index * 3 + round) % BURST_VALUES.len()]
}

// ── Non-ignored: the generated surface exists without a live database ─────────

/// The generated `react` / `reaction_of` methods type-check and can be named as
/// function items (proving the codegen ran) even where Docker is unavailable.
/// This is the #1592-style non-ignored guard for the Docker tests below.
#[test]
fn votable_methods_are_generated() {
    fn assert_is_fn<F>(_f: F) {}

    // Sum mode, pure defaults.
    assert_is_fn(<PgVotableTargetRepository as VotableTargetReactions>::react);
    assert_is_fn(<PgVotableTargetRepository as VotableTargetReactions>::reaction_of);
    // Sum mode + soft delete: proves the `deleted_at IS NULL` branch of the
    // codegen monomorphizes.
    assert_is_fn(<PgVotableSoftTargetRepository as VotableSoftTargetReactions>::react);
    assert_is_fn(<PgVotableSoftTargetRepository as VotableSoftTargetReactions>::reaction_of);
    // Count mode: `react` takes no `value` parameter.
    assert_is_fn(<PgVotableLikeTargetRepository as VotableLikeTargetReactions>::react);
    assert_is_fn(<PgVotableLikeTargetRepository as VotableLikeTargetReactions>::reaction_of);
    // Count mode + soft delete: the `deleted_at IS NULL` branch in count mode.
    assert_is_fn(<PgVotableSoftLikeTargetRepository as VotableSoftLikeTargetReactions>::react);
    assert_is_fn(
        <PgVotableSoftLikeTargetRepository as VotableSoftLikeTargetReactions>::reaction_of,
    );
    // Sum mode over an edge table whose target FK is nullable in the DDL.
    assert_is_fn(<PgVotableXorTargetRepository as VotableXorTargetReactions>::react);
    assert_is_fn(<PgVotableXorTargetRepository as VotableXorTargetReactions>::reaction_of);
}

// ── AC2: toggle / flip / insert (require Docker) ──────────────────────────────

/// AC2: the same value twice toggles the edge off; a third call re-inserts it.
/// The aggregate tracks each step.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_inserts_then_toggles_off_then_reinserts() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactor = seed_reactor(&mut conn, "ada").await;
    let target = seed_target(&mut conn, "votable_targets", "toggle").await;

    let first: Reaction = repo.react(reactor, target, 1).await.expect("insert");
    assert_eq!(first.outcome, ReactionOutcome::Inserted);
    assert_eq!(first.value, Some(1));
    assert_eq!(first.aggregate, 1);

    let second = repo.react(reactor, target, 1).await.expect("toggle off");
    assert_eq!(second.outcome, ReactionOutcome::Removed);
    assert_eq!(second.value, None);
    assert_eq!(second.aggregate, 0);
    assert_eq!(
        edge_count(&mut conn, "votes", "votable_target_id", reactor, target).await,
        0,
        "toggle-off deletes the edge"
    );

    let third = repo.react(reactor, target, 1).await.expect("re-insert");
    assert_eq!(third.outcome, ReactionOutcome::Inserted);
    assert_eq!(third.value, Some(1));
    assert_eq!(third.aggregate, 1);

    let snap = sum_snapshot(&mut conn, target).await;
    assert_eq!(snap.persisted, snap.ground_truth);
    assert_eq!(snap.persisted, 1);
}

/// AC2: a different value replaces the edge in place — never a second row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_flips_direction_without_duplicating_the_edge() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactor = seed_reactor(&mut conn, "ada").await;
    let target = seed_target(&mut conn, "votable_targets", "flip").await;

    repo.react(reactor, target, 1).await.expect("upvote");
    let flipped = repo.react(reactor, target, -1).await.expect("downvote");

    assert_eq!(flipped.outcome, ReactionOutcome::Flipped);
    assert_eq!(flipped.value, Some(-1));
    assert_eq!(flipped.aggregate, -1);
    assert_eq!(
        edge_count(&mut conn, "votes", "votable_target_id", reactor, target).await,
        1,
        "a flip replaces the edge, it does not add one"
    );

    let snap = sum_snapshot(&mut conn, target).await;
    assert_eq!(snap.persisted, -1);
    assert_eq!(snap.persisted, snap.ground_truth);
}

/// AC3: after a mixed sequence across several reactors, the persisted aggregate
/// equals the ground-truth `SUM(value)` read directly from the edge table.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_maintains_the_aggregate_equal_to_ground_truth() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactors = seed_reactors(&mut conn, 3).await;
    let target = seed_target(&mut conn, "votable_targets", "mixed").await;

    // insert, insert, insert / flip / toggle-off / re-insert.
    repo.react(reactors[0], target, 1).await.expect("r0 up");
    repo.react(reactors[1], target, 1).await.expect("r1 up");
    repo.react(reactors[2], target, -1).await.expect("r2 down");
    repo.react(reactors[1], target, -1).await.expect("r1 flip");
    repo.react(reactors[0], target, 1).await.expect("r0 toggle");
    repo.react(reactors[0], target, -1).await.expect("r0 down");

    let snap = sum_snapshot(&mut conn, target).await;
    assert_eq!(
        snap.persisted, snap.ground_truth,
        "persisted score must equal SUM(value)"
    );
    assert_eq!(snap.persisted, -3, "three down votes");

    // The repository read sees the same value the raw SQL does.
    let rows = repo
        .find_by_title("mixed".to_string())
        .await
        .expect("read back");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].score, -3);
}

/// AC2 + the issue's success metric: 50 simultaneous clicks on the same
/// `(reactor, target)` pair, then 51 more.
///
/// The outcome is fully determined, not merely bounded. The target-row lock
/// serialises the whole read-decide-write-recompute window, so N concurrent
/// same-value clicks are exactly N sequential toggles from the pre-state: an
/// **even** N must end with no edge and a zero aggregate, an **odd** N with one
/// edge and an aggregate of `+1`. Asserting only `edges ∈ {0, 1}` would pass
/// for a broken implementation that dropped or duplicated clicks, so both
/// parities are pinned exactly — 50 (even) and then a further 51 (odd) on the
/// same pair.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_is_race_safe_under_50_concurrent_same_pair_clicks() {
    async fn burst(repo: &PgVotableTargetRepository, reactor: i64, target: i64, clicks: usize) {
        let mut handles = Vec::with_capacity(clicks);
        for _ in 0..clicks {
            let repo = repo.clone();
            handles.push(tokio::spawn(
                async move { repo.react(reactor, target, 1).await },
            ));
        }
        for h in handles {
            h.await
                .expect("task did not panic")
                .expect("no unique-violation (23505) or lost update may surface to any caller");
        }
    }

    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactor = seed_reactor(&mut conn, "ada").await;
    let target = seed_target(&mut conn, "votable_targets", "contended").await;

    // Even round: 50 serialized toggles from empty must land back on empty.
    burst(&repo, reactor, target, 50).await;

    let edges = edge_count(&mut conn, "votes", "votable_target_id", reactor, target).await;
    assert_eq!(
        edges, 0,
        "50 serialized toggles from empty must end at 0 edges, got {edges}"
    );
    let snap = sum_snapshot(&mut conn, target).await;
    assert_eq!(
        snap.persisted, 0,
        "50 toggles from empty leave the persisted score at 0"
    );
    assert_eq!(
        snap.persisted, snap.ground_truth,
        "score must equal SUM(value) after the race"
    );

    // Odd round: 51 more toggles must land on exactly one +1 edge. This is the
    // discriminator — a lost or duplicated click flips the parity and fails.
    burst(&repo, reactor, target, 51).await;

    let edges = edge_count(&mut conn, "votes", "votable_target_id", reactor, target).await;
    assert_eq!(
        edges, 1,
        "51 further serialized toggles must end at exactly 1 edge, got {edges}"
    );
    let snap = sum_snapshot(&mut conn, target).await;
    assert_eq!(
        snap.persisted, 1,
        "an odd number of +1 toggles leaves the persisted score at exactly 1"
    );
    assert_eq!(
        snap.persisted, snap.ground_truth,
        "score must equal SUM(value) after the odd-parity race"
    );
}

/// AC3: 32 reactors × 4 rounds of deterministic ±1, all in flight at once
/// against a single target, checked against a **closed-form** expected total.
///
/// Each reactor only ever touches its own edge, and `react()` is a pure
/// function of that edge's current value, so every reactor's final value — and
/// therefore the final `SUM` — is fixed no matter how the rounds interleave.
/// The test replays the same toggle rule the tasks drive and asserts the
/// persisted score equals that number, rather than only asserting
/// `persisted == ground_truth` (which `0 == 0` also satisfies).
///
/// Honest scope: this detects a lost update from a stale `SUM` only
/// *probabilistically* — a lock-free implementation fails it when two reactors'
/// recomputes overlap, which is likely at this width but not guaranteed. The
/// sharper, non-probabilistic atomicity check is the single-snapshot reader
/// test below.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_aggregate_is_exact_under_randomized_multi_reactor_burst() {
    const REACTORS: usize = 32;
    const ROUNDS: usize = 4;

    /// The toggle rule `react()` implements, replayed in memory: the same value
    /// again removes the edge, a different value flips it, none inserts it.
    fn expected_final_sum() -> i64 {
        let mut total = 0i64;
        for index in 0..REACTORS {
            let mut current: Option<i16> = None;
            for round in 0..ROUNDS {
                let v = burst_value(index, round);
                current = if current == Some(v) { None } else { Some(v) };
            }
            total += i64::from(current.unwrap_or(0));
        }
        total
    }

    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactors = seed_reactors(&mut conn, REACTORS).await;
    let target = seed_target(&mut conn, "votable_targets", "burst").await;

    let mut handles = Vec::with_capacity(REACTORS);
    for (index, reactor) in reactors.iter().copied().enumerate() {
        let repo = repo.clone();
        handles.push(tokio::spawn(async move {
            for round in 0..ROUNDS {
                repo.react(reactor, target, burst_value(index, round))
                    .await
                    .expect("react must not error under contention");
            }
        }));
    }
    for h in handles {
        h.await.expect("burst task did not panic");
    }

    let snap = sum_snapshot(&mut conn, target).await;
    assert_eq!(
        snap.persisted, snap.ground_truth,
        "persisted score ({}) must equal ground-truth SUM(value) ({}) after a \
         {REACTORS}x{ROUNDS} multi-reactor burst",
        snap.persisted, snap.ground_truth
    );
    let expected = expected_final_sum();
    assert_eq!(
        snap.persisted, expected,
        "the burst's outcome is closed-form: every reactor's final value is \
         fixed by the toggle rule regardless of interleaving, so the persisted \
         score must be exactly {expected}"
    );

    // Every reactor holds at most one edge.
    for reactor in reactors {
        let edges = edge_count(&mut conn, "votes", "votable_target_id", reactor, target).await;
        assert!(
            (0..=1).contains(&edges),
            "reactor {reactor} holds {edges} edges"
        );
    }
}

/// AC3: a reader outside the reaction transaction never observes edge/aggregate
/// disagreement. The observer reads `(score, SUM(value))` in a **single** SQL
/// statement, so both values come from one snapshot; any mismatch is a real
/// atomicity violation, not a read skew artefact.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_never_lets_a_reader_observe_edge_aggregate_disagreement() {
    const REACTORS: usize = 8;
    const ROUNDS: usize = 6;

    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactors = seed_reactors(&mut conn, REACTORS).await;
    let target = seed_target(&mut conn, "votable_targets", "observed").await;
    drop(conn);

    let stop = Arc::new(AtomicBool::new(false));
    // Published after every sample so the main task can read, at the instant the
    // writers finish, how many samples were taken *while writing was still in
    // flight*. Without that floor a run in which the observer sampled once,
    // after the burst, would pass vacuously.
    let sample_count = Arc::new(AtomicUsize::new(0));
    let observer = tokio::spawn({
        let pool = pool.clone();
        let stop = Arc::clone(&stop);
        let sample_count = Arc::clone(&sample_count);
        async move {
            let mut conn = pool.get().await.expect("observer conn");
            let mut samples = 0usize;
            let mut mismatch: Option<(i64, i64)> = None;
            // Fully-qualified: `diesel_async::RunQueryDsl`'s blanket `load` is
            // in scope and would otherwise win method resolution on the `Arc`.
            while !AtomicBool::load(&stop, Ordering::SeqCst) {
                let snap = sum_snapshot(&mut conn, target).await;
                samples += 1;
                AtomicUsize::store(&sample_count, samples, Ordering::SeqCst);
                if snap.persisted != snap.ground_truth {
                    mismatch = Some((snap.persisted, snap.ground_truth));
                    break;
                }
                tokio::task::yield_now().await;
            }
            (samples, mismatch)
        }
    });

    let mut handles = Vec::with_capacity(REACTORS);
    for (index, reactor) in reactors.iter().copied().enumerate() {
        let repo = repo.clone();
        handles.push(tokio::spawn(async move {
            for round in 0..ROUNDS {
                repo.react(reactor, target, burst_value(index, round))
                    .await
                    .expect("react");
            }
        }));
    }
    for h in handles {
        h.await.expect("writer task did not panic");
    }
    // Read the sample counter *before* stopping the observer: every sample it
    // counts was taken while at least one writer was still in flight.
    // Fully-qualified for the same reason the observer's read is: diesel's
    // blanket `load` would otherwise win method resolution on the `Arc`.
    let during_writes = AtomicUsize::load(&sample_count, Ordering::SeqCst);
    stop.store(true, Ordering::SeqCst);

    let (samples, mismatch) = observer.await.expect("observer task did not panic");
    assert!(
        mismatch.is_none(),
        "a reader observed (score, SUM(value)) = {mismatch:?} in one snapshot \
         after {samples} samples — the edge mutation and the aggregate update \
         are not in the same transaction"
    );
    assert!(
        during_writes > 32,
        "the observer must have sampled the target repeatedly *during* the \
         {REACTORS}x{ROUNDS} write burst for the absence of a mismatch to mean \
         anything; only {during_writes} of {samples} samples landed there"
    );
}

// ── AC4: reaction_of ──────────────────────────────────────────────────────────

/// AC4: `reaction_of` reports the reactor's current value.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reaction_of_returns_the_reactors_current_value() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactor = seed_reactor(&mut conn, "ada").await;
    let target = seed_target(&mut conn, "votable_targets", "current").await;

    assert_eq!(
        repo.reaction_of(reactor, target)
            .await
            .expect("no edge yet"),
        None
    );

    repo.react(reactor, target, 1).await.expect("upvote");
    assert_eq!(
        repo.reaction_of(reactor, target).await.expect("after up"),
        Some(1)
    );

    repo.react(reactor, target, -1).await.expect("flip");
    assert_eq!(
        repo.reaction_of(reactor, target).await.expect("after down"),
        Some(-1)
    );
}

/// AC4: after a toggle-off the reactor has no reaction.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reaction_of_is_none_after_toggle_off() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactor = seed_reactor(&mut conn, "ada").await;
    let target = seed_target(&mut conn, "votable_targets", "toggled").await;

    repo.react(reactor, target, 1).await.expect("upvote");
    repo.react(reactor, target, 1).await.expect("toggle off");

    assert_eq!(
        repo.reaction_of(reactor, target)
            .await
            .expect("after toggle"),
        None
    );
}

/// AC4: one reactor's vote is invisible to another.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn reaction_of_is_scoped_to_the_reactor() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let ada = seed_reactor(&mut conn, "ada").await;
    let bob = seed_reactor(&mut conn, "bob").await;
    let target = seed_target(&mut conn, "votable_targets", "scoped").await;

    repo.react(bob, target, -1).await.expect("bob downvotes");

    assert_eq!(
        repo.reaction_of(ada, target).await.expect("ada"),
        None,
        "bob's vote must be invisible to ada"
    );
    assert_eq!(repo.reaction_of(bob, target).await.expect("bob"), Some(-1));
    // The aggregate still reflects bob's vote.
    let snap = sum_snapshot(&mut conn, target).await;
    assert_eq!(snap.persisted, -1);
}

// ── AC6: soft delete ──────────────────────────────────────────────────────────

/// AC6: reacting to a soft-deleted target is `NotFound`, creates no edge, and
/// leaves the persisted aggregate exactly as it was.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_on_soft_deleted_target_is_not_found_and_leaves_the_aggregate_untouched() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableSoftTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let ada = seed_reactor(&mut conn, "ada").await;
    let bob = seed_reactor(&mut conn, "bob").await;
    let target = seed_target(&mut conn, "votable_soft_targets", "gone").await;

    // A live target reacts normally.
    repo.react(ada, target, 1).await.expect("live target");
    assert_eq!(soft_target_score(&mut conn, target).await, 1);

    diesel::sql_query("UPDATE votable_soft_targets SET deleted_at = NOW() WHERE id = $1")
        .bind::<BigInt, _>(target)
        .execute(&mut conn)
        .await
        .expect("soft delete");

    let err = repo
        .react(bob, target, 1)
        .await
        .expect_err("a soft-deleted target must not accept reactions");
    assert_eq!(err.status(), StatusCode::NOT_FOUND);

    assert_eq!(
        soft_target_score(&mut conn, target).await,
        1,
        "the aggregate of a soft-deleted target is untouched"
    );
    assert_eq!(
        edge_count(
            &mut conn,
            "votable_soft_votes",
            "votable_soft_target_id",
            bob,
            target
        )
        .await,
        0,
        "no edge is created against a soft-deleted target"
    );
}

/// Reacting to a target that does not exist is `NotFound`, not a foreign-key
/// error surfaced as a 500.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_on_missing_target_is_not_found() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactor = seed_reactor(&mut conn, "ada").await;

    let err = repo
        .react(reactor, 987_654, 1)
        .await
        .expect_err("missing target must be NotFound");
    assert_eq!(err.status(), StatusCode::NOT_FOUND);
}

// ── AC1/2/3: count mode ───────────────────────────────────────────────────────

/// AC1 + AC2 + AC3 in count mode: `react` takes no value, membership toggles,
/// and `like_count` tracks `COUNT(*)`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn count_mode_react_toggles_membership_and_maintains_the_count() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableLikeTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let ada = seed_reactor(&mut conn, "ada").await;
    let bob = seed_reactor(&mut conn, "bob").await;
    let target = seed_target(&mut conn, "votable_like_targets", "liked").await;

    let first = repo.react(ada, target).await.expect("ada likes");
    assert_eq!(first.outcome, ReactionOutcome::Inserted);
    assert_eq!(first.value, Some(1), "count mode reports Some(1)");
    assert_eq!(first.aggregate, 1);
    assert_eq!(
        repo.reaction_of(ada, target).await.expect("ada's reaction"),
        Some(1)
    );

    let second = repo.react(bob, target).await.expect("bob likes");
    assert_eq!(second.aggregate, 2);

    // A repeat click removes the membership row (toggle-off), never a flip.
    let third = repo.react(ada, target).await.expect("ada unlikes");
    assert_eq!(third.outcome, ReactionOutcome::Removed);
    assert_eq!(third.value, None);
    assert_eq!(third.aggregate, 1);
    assert_eq!(
        repo.reaction_of(ada, target).await.expect("after unlike"),
        None
    );

    let snap = count_snapshot(&mut conn, target).await;
    assert_eq!(snap.persisted, snap.ground_truth);
    assert_eq!(snap.persisted, 1);
}

/// AC2 in count mode: 50 concurrent toggles are 50 serialized toggles, so the
/// membership row is gone and `like_count` is back to `0` — the even-parity
/// outcome, asserted exactly rather than as a range.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn count_mode_react_is_race_safe_under_concurrency() {
    const CLICKS: usize = 50;

    let (pool, _container) = setup_pool().await;
    let repo = PgVotableLikeTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactor = seed_reactor(&mut conn, "ada").await;
    let target = seed_target(&mut conn, "votable_like_targets", "contended").await;

    let mut handles = Vec::with_capacity(CLICKS);
    for _ in 0..CLICKS {
        let repo = repo.clone();
        handles.push(tokio::spawn(
            async move { repo.react(reactor, target).await },
        ));
    }
    for h in handles {
        h.await
            .expect("task did not panic")
            .expect("no unique-violation (23505) may surface to any caller");
    }

    let edges = edge_count(
        &mut conn,
        "likes",
        "votable_like_target_id",
        reactor,
        target,
    )
    .await;
    assert_eq!(
        edges, 0,
        "50 serialized toggles from empty must end at 0 membership rows, got {edges}"
    );

    let snap = count_snapshot(&mut conn, target).await;
    assert_eq!(
        snap.persisted, 0,
        "an even number of toggles leaves like_count at 0"
    );
    assert_eq!(
        snap.persisted, snap.ground_truth,
        "like_count must equal COUNT(*) after the race"
    );
}

/// AC6 in count mode: the `deleted_at IS NULL` gate is emitted for
/// `aggregate = count` too. Liking a soft-deleted target is `NotFound`, adds no
/// membership row, and leaves `soft_like_count` exactly as it was.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn count_mode_react_on_soft_deleted_target_is_not_found() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableSoftLikeTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let ada = seed_reactor(&mut conn, "ada").await;
    let bob = seed_reactor(&mut conn, "bob").await;
    let target = seed_target(&mut conn, "votable_soft_like_targets", "gone").await;

    // A live target likes normally.
    let liked = repo.react(ada, target).await.expect("live target");
    assert_eq!(liked.outcome, ReactionOutcome::Inserted);
    assert_eq!(liked.aggregate, 1);
    assert_eq!(soft_like_count(&mut conn, target).await, 1);

    diesel::sql_query("UPDATE votable_soft_like_targets SET deleted_at = NOW() WHERE id = $1")
        .bind::<BigInt, _>(target)
        .execute(&mut conn)
        .await
        .expect("soft delete");

    let err = repo
        .react(bob, target)
        .await
        .expect_err("a soft-deleted target must not accept likes");
    assert_eq!(err.status(), StatusCode::NOT_FOUND);

    assert_eq!(
        soft_like_count(&mut conn, target).await,
        1,
        "the count of a soft-deleted target is untouched"
    );
    assert_eq!(
        edge_count(
            &mut conn,
            "soft_likes",
            "votable_soft_like_target_id",
            bob,
            target
        )
        .await,
        0,
        "no membership row is created against a soft-deleted target"
    );
}

// ── Nullable target FK in the DDL (reddit-clone's XOR shape) ──────────────────

/// The edge table's target FK is `NULL`-able in the real DDL and already holds
/// `NULL`-target rows *before* the first `react()`, while the generated hidden
/// `table!` declares that column non-nullable.
///
/// Two properties are at stake and both are asserted here: the pre-existing
/// `NULL`-target rows never contaminate the aggregate (they are excluded by the
/// `WHERE target_fk = $t` recompute and are invisible to `reaction_of`), and
/// they never block a reactor from also holding a real edge — because a
/// Postgres unique constraint treats `NULL`s as distinct, so
/// `UNIQUE (reactor, target)` only constrains the non-`NULL` rows.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_is_exact_when_the_edge_table_has_a_nullable_target_fk() {
    let (pool, _container) = setup_pool().await;
    let repo = PgVotableXorTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let ada = seed_reactor(&mut conn, "ada").await;
    let bob = seed_reactor(&mut conn, "bob").await;
    let target = seed_target(&mut conn, "votable_xor_targets", "xor").await;

    // The sibling target kind, and two edges pointing at it — written *before*
    // any react() call, so every statement below runs with NULL target FKs
    // already in the table.
    let other = diesel::sql_query("INSERT INTO votable_xor_others DEFAULT VALUES RETURNING id")
        .get_result::<IdRow>(&mut conn)
        .await
        .expect("seed xor sibling")
        .id;
    for reactor in [ada, bob] {
        diesel::sql_query(
            "INSERT INTO votable_xor_votes (votable_reactor_id, votable_xor_other_id, value) \
             VALUES ($1, $2, 1)",
        )
        .bind::<BigInt, _>(reactor)
        .bind::<BigInt, _>(other)
        .execute(&mut conn)
        .await
        .expect("seed NULL-target edge");
    }

    // A reactor that already owns a NULL-target row can still insert, flip and
    // toggle a real edge: the unique constraint does not see the NULL rows.
    let first = repo.react(ada, target, 1).await.expect("insert");
    assert_eq!(first.outcome, ReactionOutcome::Inserted);
    assert_eq!(first.aggregate, 1, "the NULL-target rows are not summed in");

    let flipped = repo.react(ada, target, -1).await.expect("flip");
    assert_eq!(flipped.outcome, ReactionOutcome::Flipped);
    assert_eq!(flipped.aggregate, -1);

    let bobs = repo.react(bob, target, 1).await.expect("bob");
    assert_eq!(bobs.aggregate, 0);

    let removed = repo.react(ada, target, -1).await.expect("toggle off");
    assert_eq!(removed.outcome, ReactionOutcome::Removed);
    assert_eq!(removed.aggregate, 1);

    let snap = xor_snapshot(&mut conn, target).await;
    assert_eq!(snap.persisted, snap.ground_truth);
    assert_eq!(snap.persisted, 1, "only bob's +1 targets this row");

    // `reaction_of` reports the real edge, never the NULL-target one.
    assert_eq!(repo.reaction_of(ada, target).await.expect("ada"), None);
    assert_eq!(repo.reaction_of(bob, target).await.expect("bob"), Some(1));

    // The NULL-target rows are still there, untouched by any of the above.
    let orphans = diesel::sql_query(
        "SELECT COUNT(*)::BIGINT AS count FROM votable_xor_votes \
         WHERE votable_xor_target_id IS NULL AND value = 1",
    )
    .get_result::<CountRow>(&mut conn)
    .await
    .expect("count NULL-target edges")
    .count;
    assert_eq!(orphans, 2, "react() never touches the sibling kind's rows");
}
