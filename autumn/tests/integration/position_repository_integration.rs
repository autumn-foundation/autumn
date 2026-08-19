//! Integration tests for the `position(...)` repository option (issue #1358):
//! `move_to`/`move_before`/`move_after`/`move_up`/`move_down` against a real
//! Postgres database, including the concurrent-reorder invariant the issue's
//! Success Metric calls for — "the position invariant... holds under a
//! concurrent-reorder property test".
//!
//! Run with:
//!
//!     cargo test -p autumn-web --test integration_tests position_repository -- --ignored --include-ignored

#![cfg(all(feature = "db", feature = "test-support"))]

use autumn_web::reexports::diesel::prelude::*;
use autumn_web::reexports::diesel_async::RunQueryDsl;
use autumn_web::test::TestDb;

mod schema {
    autumn_web::reexports::diesel::table! {
        position_tasks (id) {
            id -> Int8,
            title -> Text,
            rank -> Int8,
            board_id -> Int8,
        }
    }
}

use schema::position_tasks;

#[autumn_web::model(table = "position_tasks")]
pub struct PositionTask {
    #[id]
    pub id: i64,
    pub title: String,
    #[position]
    pub rank: i64,
    pub board_id: i64,
}

// Scoped to `board_id`: every test below picks its own `board_id` value so
// tests running in the shared container never share (and therefore never
// race on) a scope with each other.
#[autumn_web::repository(
    PositionTask,
    table = "position_tasks",
    position(column = "rank", scope = "board_id")
)]
pub trait PositionTaskRepository {}

static SETUP_CELL: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn setup_table(db: &TestDb) {
    SETUP_CELL
        .get_or_init(|| async {
            db.execute_sql(
                "CREATE TABLE IF NOT EXISTS position_tasks (
                    id BIGSERIAL PRIMARY KEY,
                    title TEXT NOT NULL,
                    rank BIGINT NOT NULL,
                    board_id BIGINT NOT NULL
                )",
            )
            .await;
        })
        .await;
}

type Pool = autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool<
    autumn_web::RuntimeConnection,
>;

/// Seed `count` rows in `board_id`, with a dense `0..count` starting order,
/// returning their ids in that order (`ids[i]` is the row currently at
/// position `i`). Assumes `board_id` is not shared with any other test. Goes
/// straight through the pool (not the repository) since inserting a row with
/// a pre-chosen `rank` is exactly what `#[position]` excludes from `New*` —
/// production code never does this; only test seeding needs it.
async fn seed(pool: &Pool, board_id: i64, count: i64) -> Vec<i64> {
    let mut conn = pool.get().await.expect("checkout connection");
    let mut ids = Vec::with_capacity(usize::try_from(count).expect("count fits in usize"));
    for i in 0..count {
        let id: i64 = diesel::insert_into(position_tasks::table)
            .values((
                position_tasks::title.eq(format!("task-{i}")),
                position_tasks::rank.eq(i),
                position_tasks::board_id.eq(board_id),
            ))
            .returning(position_tasks::id)
            .get_result(&mut conn)
            .await
            .expect("seed insert");
        ids.push(id);
    }
    ids
}

/// Positions of every row in `board_id`, ordered by id (matching `seed`'s
/// insertion order) — `(id, rank)` pairs.
async fn ranks_by_id(pool: &Pool, board_id: i64) -> Vec<(i64, i64)> {
    let mut conn = pool.get().await.expect("checkout connection");
    position_tasks::table
        .filter(position_tasks::board_id.eq(board_id))
        .select((position_tasks::id, position_tasks::rank))
        .order(position_tasks::id.asc())
        .load(&mut conn)
        .await
        .expect("load ranks")
}

/// Rows in `board_id`, ordered by their current `rank` — the visible order a
/// reorderable list would render.
async fn ordered_titles(pool: &Pool, board_id: i64) -> Vec<String> {
    let mut conn = pool.get().await.expect("checkout connection");
    position_tasks::table
        .filter(position_tasks::board_id.eq(board_id))
        .select(position_tasks::title)
        .order(position_tasks::rank.asc())
        .load(&mut conn)
        .await
        .expect("load ordered titles")
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn move_to_shifts_only_the_rows_between_source_and_destination() {
    let db = TestDb::shared().await;
    setup_table(db).await;
    let repo = PgPositionTaskRepository::with_pool_untracked(db.pool());
    let board_id = 1001;
    let ids = seed(&db.pool(), board_id, 5).await; // A B C D E at 0..4

    // Move A (index 0) to index 3: expect B C D A E.
    repo.move_to(ids[0], 3).await.expect("move_to");
    let titles = ordered_titles(&db.pool(), board_id).await;
    assert_eq!(
        titles,
        vec!["task-1", "task-2", "task-3", "task-0", "task-4"]
    );

    // Invariant: still a dense 0..4 permutation, no duplicates or gaps.
    let mut ranks: Vec<i64> = ranks_by_id(&db.pool(), board_id)
        .await
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    ranks.sort_unstable();
    assert_eq!(ranks, vec![0, 1, 2, 3, 4]);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn move_to_clamps_out_of_range_targets() {
    let db = TestDb::shared().await;
    setup_table(db).await;
    let repo = PgPositionTaskRepository::with_pool_untracked(db.pool());
    let board_id = 1002;
    let ids = seed(&db.pool(), board_id, 3).await;

    repo.move_to(ids[0], 999)
        .await
        .expect("move_to clamps high");
    assert_eq!(
        ordered_titles(&db.pool(), board_id).await,
        vec!["task-1", "task-2", "task-0"]
    );

    repo.move_to(ids[2], -50).await.expect("move_to clamps low");
    assert_eq!(
        ordered_titles(&db.pool(), board_id).await,
        vec!["task-0", "task-1", "task-2"]
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn move_up_and_move_down_step_by_one() {
    let db = TestDb::shared().await;
    setup_table(db).await;
    let repo = PgPositionTaskRepository::with_pool_untracked(db.pool());
    let board_id = 1003;
    let ids = seed(&db.pool(), board_id, 3).await; // A B C

    repo.move_down(ids[0]).await.expect("move_down");
    assert_eq!(
        ordered_titles(&db.pool(), board_id).await,
        vec!["task-1", "task-0", "task-2"]
    );

    repo.move_up(ids[0]).await.expect("move_up");
    assert_eq!(
        ordered_titles(&db.pool(), board_id).await,
        vec!["task-0", "task-1", "task-2"]
    );

    // A no-op at the boundary rather than an error.
    repo.move_up(ids[0])
        .await
        .expect("move_up at start is a no-op");
    assert_eq!(
        ordered_titles(&db.pool(), board_id).await,
        vec!["task-0", "task-1", "task-2"]
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn move_before_and_move_after_in_both_directions() {
    let db = TestDb::shared().await;
    setup_table(db).await;
    let repo = PgPositionTaskRepository::with_pool_untracked(db.pool());
    let board_id = 1004;
    let ids = seed(&db.pool(), board_id, 5).await; // A B C D E

    // Move E (last) after B: A B E C D.
    repo.move_after(ids[4], ids[1])
        .await
        .expect("move_after (moving up)");
    assert_eq!(
        ordered_titles(&db.pool(), board_id).await,
        vec!["task-0", "task-1", "task-4", "task-2", "task-3"]
    );

    // Move B (now index 1) after D (now index 4): A E C D B.
    repo.move_after(ids[1], ids[3])
        .await
        .expect("move_after (moving down)");
    assert_eq!(
        ordered_titles(&db.pool(), board_id).await,
        vec!["task-0", "task-4", "task-2", "task-3", "task-1"]
    );

    // Move B (now last) before E (now index 1): A B E C D.
    repo.move_before(ids[1], ids[4])
        .await
        .expect("move_before (moving up)");
    assert_eq!(
        ordered_titles(&db.pool(), board_id).await,
        vec!["task-0", "task-1", "task-4", "task-2", "task-3"]
    );

    // Move A (index 0) before D (index 4): B E C A D.
    repo.move_before(ids[0], ids[3])
        .await
        .expect("move_before (moving down)");
    assert_eq!(
        ordered_titles(&db.pool(), board_id).await,
        vec!["task-1", "task-4", "task-2", "task-0", "task-3"]
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn move_before_rejects_a_different_scope() {
    let db = TestDb::shared().await;
    setup_table(db).await;
    let repo = PgPositionTaskRepository::with_pool_untracked(db.pool());
    let board_a = seed(&db.pool(), 1005, 2).await;
    let board_b = seed(&db.pool(), 1006, 2).await;

    let err = repo
        .move_before(board_a[0], board_b[0])
        .await
        .expect_err("cross-scope move_before must be rejected");
    assert!(err.to_string().contains("scope"), "unexpected error: {err}");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn moves_in_one_scope_never_affect_another_scope() {
    let db = TestDb::shared().await;
    setup_table(db).await;
    let repo = PgPositionTaskRepository::with_pool_untracked(db.pool());
    let board_a = seed(&db.pool(), 1007, 3).await;
    let _board_b = seed(&db.pool(), 1008, 3).await;

    repo.move_to(board_a[0], 2).await.expect("move in board A");

    assert_eq!(
        ordered_titles(&db.pool(), 1007).await,
        vec!["task-1", "task-2", "task-0"],
        "board A reordered"
    );
    assert_eq!(
        ordered_titles(&db.pool(), 1008).await,
        vec!["task-0", "task-1", "task-2"],
        "board B must be untouched by a move scoped to board A"
    );
}

/// The issue's Success Metric: "The position invariant — contiguous
/// `0..len-1` per scope — holds under a concurrent-reorder property test."
///
/// Spawns many concurrent `move_to`/`move_up`/`move_down` calls (from
/// separate tokio tasks, each with its own connection, genuinely
/// interleaved by Postgres — not serialized by test-harness transactional
/// isolation) against the same scope, then asserts the stored positions are
/// still an exact `0..len-1` permutation: no duplicate position, no gap,
/// regardless of the interleaving Postgres actually chose.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_moves_never_produce_duplicate_or_gapped_positions() {
    const COUNT: i64 = 12;

    let db = TestDb::shared().await;
    setup_table(db).await;
    let board_id = 1009;
    let ids = seed(&db.pool(), board_id, COUNT).await;

    let mut handles = Vec::new();
    for round in 0_usize..40 {
        let repo = PgPositionTaskRepository::with_pool_untracked(db.pool());
        let ids = ids.clone();
        handles.push(tokio::spawn(async move {
            let id = ids[(round * 7 + 3) % ids.len()];
            match round % 3 {
                0 => {
                    let target = i64::try_from(round * 5).expect("fits i64") % COUNT;
                    repo.move_to(id, target).await
                }
                1 => repo.move_up(id).await,
                _ => repo.move_down(id).await,
            }
        }));
    }
    for handle in handles {
        handle
            .await
            .expect("task panicked")
            .expect("move_* must not error under concurrency");
    }

    let mut ranks: Vec<i64> = ranks_by_id(&db.pool(), board_id)
        .await
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    ranks.sort_unstable();
    let expected: Vec<i64> = (0..COUNT).collect();
    assert_eq!(
        ranks, expected,
        "positions must remain an exact 0..len-1 permutation after concurrent moves \
         (no duplicates, no gaps), got: {ranks:?}"
    );
}
