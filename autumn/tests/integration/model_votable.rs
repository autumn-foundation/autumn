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
//!
//! `VotableTarget` deliberately exercises **pure defaults** — `table` resolves
//! to `pluralize("vote") == "votes"`, `reactor_fk` to `votable_reactor_id`,
//! `target_fk` to `votable_target_id`, `value_column` to `value` and `column`
//! to `score`. The other two fixtures must override `table` (or `name`, which
//! drives it) because three votable models in one test binary would otherwise
//! all resolve onto the same `votes` edge table.
//!
//! The composite `UNIQUE (reactor_fk, target_fk)` on every edge table is
//! load-bearing: it is the `ON CONFLICT` arbiter the generated upsert names,
//! and it is what makes "at most one edge per (reactor, target)" a database
//! guarantee rather than an application convention.
//!
//! Concurrency assertions are **invariant-based**, never timing-based: edge
//! count ∈ {0, 1} and `aggregate == ground truth` hold under *every*
//! interleaving, so the tests cannot flake on scheduling.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

// ── Setup & helpers ───────────────────────────────────────────────────────────

/// Every DDL statement the fixtures need. The edge tables differ on purpose:
///
/// - `votes` carries a surrogate `id` and a `created_at`, exactly like
///   reddit-clone's shipped table, proving the generated code tolerates extra
///   columns it never names.
/// - `votable_soft_votes` and `likes` are the minimal shape (the composite key
///   only), proving no surrogate key is required.
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
/// `(reactor, target)` pair. No caller may error, the edge count must land in
/// `{0, 1}`, and the persisted aggregate must equal ground truth.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_is_race_safe_under_50_concurrent_same_pair_clicks() {
    const CLICKS: usize = 50;

    let (pool, _container) = setup_pool().await;
    let repo = PgVotableTargetRepository::with_pool_untracked(pool.clone());
    let mut conn = pool.get().await.expect("conn");
    let reactor = seed_reactor(&mut conn, "ada").await;
    let target = seed_target(&mut conn, "votable_targets", "contended").await;

    let mut handles = Vec::with_capacity(CLICKS);
    for _ in 0..CLICKS {
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

    let edges = edge_count(&mut conn, "votes", "votable_target_id", reactor, target).await;
    assert!(
        (0..=1).contains(&edges),
        "at most one edge per (reactor, target) under any interleaving, got {edges}"
    );

    let snap = sum_snapshot(&mut conn, target).await;
    assert_eq!(
        snap.persisted, snap.ground_truth,
        "score must equal SUM(value) after the race"
    );
}

/// AC3: the test a lock-free design fails. 32 reactors × 4 rounds of
/// deterministic ±1, all in flight at once against a single target. Under a
/// lock-free "upsert then recompute" the concurrent `SUM`s clobber each other
/// and the persisted score drifts permanently; under a per-target lock it is
/// exact.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn react_aggregate_is_exact_under_randomized_multi_reactor_burst() {
    const REACTORS: usize = 32;
    const ROUNDS: usize = 4;

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
    let observer = tokio::spawn({
        let pool = pool.clone();
        let stop = Arc::clone(&stop);
        async move {
            let mut conn = pool.get().await.expect("observer conn");
            let mut samples = 0usize;
            let mut mismatch: Option<(i64, i64)> = None;
            // Fully-qualified: `diesel_async::RunQueryDsl`'s blanket `load` is
            // in scope and would otherwise win method resolution on the `Arc`.
            while !AtomicBool::load(&stop, Ordering::SeqCst) {
                let snap = sum_snapshot(&mut conn, target).await;
                samples += 1;
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
    stop.store(true, Ordering::SeqCst);

    let (samples, mismatch) = observer.await.expect("observer task did not panic");
    assert!(samples > 0, "the observer must have sampled at least once");
    assert!(
        mismatch.is_none(),
        "a reader observed (score, SUM(value)) = {mismatch:?} in one snapshot \
         after {samples} samples — the edge mutation and the aggregate update \
         are not in the same transaction"
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

/// AC2 in count mode: 50 concurrent toggles converge to at most one membership
/// row and a `like_count` equal to `COUNT(*)`.
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
    assert!(
        (0..=1).contains(&edges),
        "at most one membership row per (reactor, target), got {edges}"
    );

    let snap = count_snapshot(&mut conn, target).await;
    assert_eq!(
        snap.persisted, snap.ground_truth,
        "like_count must equal COUNT(*) after the race"
    );
}
