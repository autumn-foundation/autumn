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
        vec!["task-2", "task-1", "task-0"]
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

// ── Insert-assign trigger concurrency (a separate, trigger-backed table) ───
//
// The tests above seed rows with an explicit `rank` (bypassing the insert
// trigger entirely, since a hand-declared test schema has no migration to
// install one) to isolate `move_*`'s own locking. This table instead
// installs the real trigger SQL `position_triggers_up_sql_for` generates
// (mirrored by hand here — `autumn` cannot depend on `autumn-cli`), so a
// concurrent-*insert* property test exercises the actual insert-time
// assignment path, including the `pg_advisory_xact_lock` fix for the race
// two concurrent `BEFORE INSERT` triggers' un-locked `SELECT MAX(position)`
// reads would otherwise hit (see `autumn-cli`'s `position_triggers_up_sql_for`
// doc comment).

mod triggered_schema {
    autumn_web::reexports::diesel::table! {
        position_triggered_tasks (id) {
            id -> Int8,
            title -> Text,
            rank -> Int8,
            board_id -> Int8,
        }
    }
}

use triggered_schema::position_triggered_tasks;

#[autumn_web::model(table = "position_triggered_tasks")]
pub struct PositionTriggeredTask {
    #[id]
    pub id: i64,
    pub title: String,
    #[position]
    pub rank: i64,
    pub board_id: i64,
}

#[autumn_web::repository(
    PositionTriggeredTask,
    table = "position_triggered_tasks",
    position(column = "rank", scope = "board_id")
)]
pub trait PositionTriggeredTaskRepository {}

static TRIGGERED_SETUP_CELL: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

#[allow(clippy::too_many_lines)]
async fn setup_triggered_table(db: &TestDb) {
    TRIGGERED_SETUP_CELL
        .get_or_init(|| async {
            db.execute_sql(
                "CREATE TABLE IF NOT EXISTS position_triggered_tasks (
                    id BIGSERIAL PRIMARY KEY,
                    title TEXT NOT NULL,
                    rank BIGINT NOT NULL DEFAULT 0,
                    board_id BIGINT NOT NULL
                )",
            )
            .await;
            db.execute_sql(
                "CREATE OR REPLACE FUNCTION position_triggered_tasks_rank_assign() \
                 RETURNS TRIGGER AS $$
                 BEGIN
                   PERFORM pg_advisory_xact_lock(
                       hashtext('position_triggered_tasks_rank_assign'),
                       hashtext(NEW.\"board_id\"::text)
                   );
                   NEW.\"rank\" := COALESCE(
                       (SELECT MAX(\"rank\") + 1 FROM \"position_triggered_tasks\" \
                        WHERE \"board_id\" = NEW.\"board_id\"),
                       0
                   );
                   RETURN NEW;
                 END;
                 $$ LANGUAGE plpgsql;",
            )
            .await;
            db.execute_sql(
                "DROP TRIGGER IF EXISTS position_triggered_tasks_rank_assign_trg \
                 ON \"position_triggered_tasks\";",
            )
            .await;
            db.execute_sql(
                "CREATE TRIGGER position_triggered_tasks_rank_assign_trg \
                 BEFORE INSERT ON \"position_triggered_tasks\" \
                 FOR EACH ROW EXECUTE FUNCTION position_triggered_tasks_rank_assign();",
            )
            .await;
            // Mirrors `position_triggers_up_sql_for`'s Postgres delete-compact
            // trigger exactly, including taking the *same* advisory lock key
            // as the assign trigger above — the two must fully serialize
            // against each other, not just against their own kind, or a
            // concurrent insert's unlocked `MAX(rank)` read can race a
            // compaction shift and leave a gap (see
            // `concurrent_insert_and_delete_never_leave_a_gap` below).
            db.execute_sql(
                "CREATE OR REPLACE FUNCTION position_triggered_tasks_rank_compact() \
                 RETURNS TRIGGER AS $$
                 BEGIN
                   PERFORM pg_advisory_xact_lock(
                       hashtext('position_triggered_tasks_rank_assign'),
                       hashtext(OLD.\"board_id\"::text)
                   );
                   UPDATE \"position_triggered_tasks\" SET \"rank\" = \"rank\" - 1 \
                   WHERE \"board_id\" = OLD.\"board_id\" AND \"rank\" > OLD.\"rank\";
                   RETURN OLD;
                 END;
                 $$ LANGUAGE plpgsql;",
            )
            .await;
            db.execute_sql(
                "DROP TRIGGER IF EXISTS position_triggered_tasks_rank_compact_trg \
                 ON \"position_triggered_tasks\";",
            )
            .await;
            db.execute_sql(
                "CREATE TRIGGER position_triggered_tasks_rank_compact_trg \
                 AFTER DELETE ON \"position_triggered_tasks\" \
                 FOR EACH ROW EXECUTE FUNCTION position_triggered_tasks_rank_compact();",
            )
            .await;
            // Mirrors `position_triggers_up_sql_for`'s Postgres `rescope`
            // trigger exactly (Codex review finding for issue #1358): an
            // ordinary `UPDATE` reassigning `board_id` must compact the old
            // board's gap and append the row to the end of the new board,
            // or moving a task to another board breaks the contiguous
            // invariant (see `scope_reassignment_via_update_compacts_old_and_appends_to_new`
            // below).
            db.execute_sql(
                "CREATE OR REPLACE FUNCTION position_triggered_tasks_rank_rescope() \
                 RETURNS TRIGGER AS $$
                 BEGIN
                   IF hashtext(OLD.\"board_id\"::text) <= hashtext(NEW.\"board_id\"::text) THEN
                     PERFORM pg_advisory_xact_lock(
                         hashtext('position_triggered_tasks_rank_assign'), hashtext(OLD.\"board_id\"::text)
                     );
                     PERFORM pg_advisory_xact_lock(
                         hashtext('position_triggered_tasks_rank_assign'), hashtext(NEW.\"board_id\"::text)
                     );
                   ELSE
                     PERFORM pg_advisory_xact_lock(
                         hashtext('position_triggered_tasks_rank_assign'), hashtext(NEW.\"board_id\"::text)
                     );
                     PERFORM pg_advisory_xact_lock(
                         hashtext('position_triggered_tasks_rank_assign'), hashtext(OLD.\"board_id\"::text)
                     );
                   END IF;
                   UPDATE \"position_triggered_tasks\" SET \"rank\" = \"rank\" - 1 \
                   WHERE \"board_id\" = OLD.\"board_id\" AND \"rank\" > OLD.\"rank\";
                   NEW.\"rank\" := COALESCE(
                       (SELECT MAX(\"rank\") + 1 FROM \"position_triggered_tasks\" \
                        WHERE \"board_id\" = NEW.\"board_id\"),
                       0
                   );
                   RETURN NEW;
                 END;
                 $$ LANGUAGE plpgsql;",
            )
            .await;
            db.execute_sql(
                "DROP TRIGGER IF EXISTS position_triggered_tasks_rank_rescope_trg \
                 ON \"position_triggered_tasks\";",
            )
            .await;
            db.execute_sql(
                "CREATE TRIGGER position_triggered_tasks_rank_rescope_trg \
                 BEFORE UPDATE OF \"board_id\" ON \"position_triggered_tasks\" \
                 FOR EACH ROW WHEN (NEW.\"board_id\" IS DISTINCT FROM OLD.\"board_id\") \
                 EXECUTE FUNCTION position_triggered_tasks_rank_rescope();",
            )
            .await;
        })
        .await;
}

async fn triggered_ranks(pool: &Pool, board_id: i64) -> Vec<i64> {
    let mut conn = pool.get().await.expect("checkout connection");
    position_triggered_tasks::table
        .filter(position_triggered_tasks::board_id.eq(board_id))
        .select(position_triggered_tasks::rank)
        .load(&mut conn)
        .await
        .expect("load ranks")
}

/// Regression for the concurrent-insert race the review round for issue
/// #1358 caught: without the advisory lock, two `BEFORE INSERT` triggers
/// racing on the same scope could both read the same `MAX(rank)` (a plain,
/// unlocked read) and be assigned the same value. Spawns many concurrent
/// inserts into one scope, from separate connections, and asserts the
/// stored positions are an exact `0..len-1` permutation — no duplicates, no
/// gaps — regardless of how Postgres interleaved them.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_inserts_never_produce_duplicate_or_gapped_positions() {
    const N: usize = 30;

    let db = TestDb::shared().await;
    setup_triggered_table(db).await;
    let board_id = 2001;

    let mut handles = Vec::new();
    for i in 0..N {
        let pool = db.pool();
        handles.push(tokio::spawn(async move {
            let mut conn = pool.get().await.expect("checkout connection");
            diesel::insert_into(position_triggered_tasks::table)
                .values((
                    position_triggered_tasks::title.eq(format!("concurrent-{i}")),
                    position_triggered_tasks::board_id.eq(board_id),
                ))
                .execute(&mut conn)
                .await
        }));
    }
    for handle in handles {
        handle
            .await
            .expect("task panicked")
            .expect("concurrent insert must not error");
    }

    let mut ranks = triggered_ranks(&db.pool(), board_id).await;
    ranks.sort_unstable();
    let expected: Vec<i64> = (0..i64::try_from(N).expect("fits i64")).collect();
    assert_eq!(
        ranks, expected,
        "concurrent inserts into the same scope must still produce an exact 0..len-1 \
         permutation (no duplicates, no gaps), got: {ranks:?}"
    );
}

/// Regression for the same race's cross-operation form: a concurrent insert
/// and a concurrent delete (of an unrelated row) on the same scope must not
/// leave a gap — the insert-assign and delete-compact triggers share the
/// same advisory lock key specifically so they fully serialize against each
/// other, not just against their own kind.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_insert_and_delete_never_leave_a_gap() {
    let db = TestDb::shared().await;
    setup_triggered_table(db).await;
    let board_id = 2002;

    // Seed 5 rows (0..4) through the real trigger, then delete the middle one
    // and insert a new one concurrently.
    let mut seeded_ids = Vec::new();
    for i in 0..5 {
        let mut conn = db.pool().get().await.expect("checkout connection");
        let id: i64 = diesel::insert_into(position_triggered_tasks::table)
            .values((
                position_triggered_tasks::title.eq(format!("seed-{i}")),
                position_triggered_tasks::board_id.eq(board_id),
            ))
            .returning(position_triggered_tasks::id)
            .get_result(&mut conn)
            .await
            .expect("seed insert");
        seeded_ids.push(id);
    }
    let delete_id = seeded_ids[2]; // currently at rank 2

    let delete_pool = db.pool();
    let delete_task = tokio::spawn(async move {
        let mut conn = delete_pool.get().await.expect("checkout connection");
        diesel::delete(position_triggered_tasks::table.find(delete_id))
            .execute(&mut conn)
            .await
    });
    let insert_pool = db.pool();
    let insert_task = tokio::spawn(async move {
        let mut conn = insert_pool.get().await.expect("checkout connection");
        diesel::insert_into(position_triggered_tasks::table)
            .values((
                position_triggered_tasks::title.eq("concurrent-new"),
                position_triggered_tasks::board_id.eq(board_id),
            ))
            .execute(&mut conn)
            .await
    });
    delete_task
        .await
        .expect("task panicked")
        .expect("delete must not error");
    insert_task
        .await
        .expect("task panicked")
        .expect("insert must not error");

    let mut ranks = triggered_ranks(&db.pool(), board_id).await;
    ranks.sort_unstable();
    // 5 seeded - 1 deleted + 1 inserted = 5 rows, still contiguous 0..4.
    assert_eq!(
        ranks,
        vec![0, 1, 2, 3, 4],
        "a concurrent insert and delete on the same scope must not leave a gap: {ranks:?}"
    );
}

/// Regression for the deadlock the adversarial review round for issue #1358
/// caught: `move_to`'s `SELECT ... FOR UPDATE ORDER BY id` locks every row in
/// the scope in a *fixed id order*, but the delete-compact trigger's
/// `UPDATE ... WHERE rank > $1` can lock the same rows via an index scan on
/// `rank`, i.e. **not** in `id` order — so a concurrent `move_to` and delete
/// on the same scope could lock rows in opposite orders and hit a genuine
/// Postgres deadlock (error `40P01`, surfaced as a `500`). The fix has
/// `move_to` take the same `pg_advisory_xact_lock` the triggers take, before
/// its row-locking `SELECT`, so the two fully serialize instead of racing for
/// row locks at all. Runs many rounds of a concurrent `move_to` + delete/
/// re-insert pair on one scope through the real trigger-backed repository —
/// deadlocked, either task would return `Err`, so asserting both succeed
/// every round is itself the regression check; the final contiguous-
/// permutation assertion additionally confirms the shared lock didn't trade
/// deadlock-freedom for a correctness gap.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn concurrent_move_to_and_delete_never_deadlock() {
    const COUNT: usize = 6;

    let db = TestDb::shared().await;
    setup_triggered_table(db).await;
    let board_id = 2003;

    let mut ids = Vec::with_capacity(COUNT);
    for i in 0..COUNT {
        let mut conn = db.pool().get().await.expect("checkout connection");
        let id: i64 = diesel::insert_into(position_triggered_tasks::table)
            .values((
                position_triggered_tasks::title.eq(format!("seed-{i}")),
                position_triggered_tasks::board_id.eq(board_id),
            ))
            .returning(position_triggered_tasks::id)
            .get_result(&mut conn)
            .await
            .expect("seed insert");
        ids.push(id);
    }

    for round in 0_usize..20 {
        let move_idx = round % ids.len();
        let move_id = ids[move_idx];
        // Never move and delete the *same* row in one round: if the delete
        // commits first, `move_to`'s locked-set lookup for `move_id` would
        // legitimately 404 — a test-harness race, not the deadlock this test
        // exists to catch.
        let mut delete_idx = (round * 3 + 1) % ids.len();
        if delete_idx == move_idx {
            delete_idx = (delete_idx + 1) % ids.len();
        }
        let delete_id = ids[delete_idx];

        let move_repo = PgPositionTriggeredTaskRepository::with_pool_untracked(db.pool());
        let target =
            i64::try_from(round * 5).expect("fits i64") % i64::try_from(COUNT).expect("fits i64");
        let move_task = tokio::spawn(async move { move_repo.move_to(move_id, target).await });

        let delete_pool = db.pool();
        let delete_task = tokio::spawn(async move {
            let mut conn = delete_pool.get().await.expect("checkout connection");
            diesel::delete(position_triggered_tasks::table.find(delete_id))
                .execute(&mut conn)
                .await
        });

        move_task
            .await
            .expect("move_to task panicked")
            .expect("move_to must not deadlock or otherwise error");
        delete_task
            .await
            .expect("delete task panicked")
            .expect("delete must not deadlock or otherwise error");

        // Re-insert a replacement so the scope's row count (and therefore
        // every later round's `target`/`delete_idx` arithmetic) stays
        // stable across rounds.
        let mut conn = db.pool().get().await.expect("checkout connection");
        let new_id: i64 = diesel::insert_into(position_triggered_tasks::table)
            .values((
                position_triggered_tasks::title.eq(format!("round-{round}")),
                position_triggered_tasks::board_id.eq(board_id),
            ))
            .returning(position_triggered_tasks::id)
            .get_result(&mut conn)
            .await
            .expect("replacement insert");
        ids[delete_idx] = new_id;
    }

    let mut ranks = triggered_ranks(&db.pool(), board_id).await;
    ranks.sort_unstable();
    let expected: Vec<i64> = (0..i64::try_from(COUNT).expect("fits i64")).collect();
    assert_eq!(
        ranks, expected,
        "positions must remain an exact 0..len-1 permutation after concurrent move_to + \
         delete/re-insert rounds (no duplicates, no gaps), got: {ranks:?}"
    );
}

/// Regression for a Codex review finding on issue #1358: an ordinary
/// `UPDATE` reassigning a scoped row's scope FK (e.g. dragging a Kanban
/// card to a different board via `board_id`) is not itself a `move_to`
/// call, an insert, or a delete — nothing else in the generated code path
/// would have compacted the row's old board or assigned it a fresh
/// position in the new one. Seeds two boards through the real trigger,
/// moves the middle row of board A to board B via a plain `diesel::update`,
/// and asserts: board A compacts (no gap where the row used to be) and
/// board B appends the row at the end (no duplicate position).
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn scope_reassignment_via_update_compacts_old_and_appends_to_new() {
    let db = TestDb::shared().await;
    setup_triggered_table(db).await;
    let board_a = 2004;
    let board_b = 2005;

    let mut a_ids = Vec::new();
    for i in 0..5 {
        let mut conn = db.pool().get().await.expect("checkout connection");
        let id: i64 = diesel::insert_into(position_triggered_tasks::table)
            .values((
                position_triggered_tasks::title.eq(format!("a-{i}")),
                position_triggered_tasks::board_id.eq(board_a),
            ))
            .returning(position_triggered_tasks::id)
            .get_result(&mut conn)
            .await
            .expect("seed board a");
        a_ids.push(id);
    }
    for i in 0..3 {
        let mut conn = db.pool().get().await.expect("checkout connection");
        diesel::insert_into(position_triggered_tasks::table)
            .values((
                position_triggered_tasks::title.eq(format!("b-{i}")),
                position_triggered_tasks::board_id.eq(board_b),
            ))
            .execute(&mut conn)
            .await
            .expect("seed board b");
    }

    let moved_id = a_ids[2]; // currently rank 2 in board A
    let mut conn = db.pool().get().await.expect("checkout connection");
    diesel::update(position_triggered_tasks::table.find(moved_id))
        .set(position_triggered_tasks::board_id.eq(board_b))
        .execute(&mut conn)
        .await
        .expect("reassign board_id must not error");

    let mut a_ranks = triggered_ranks(&db.pool(), board_a).await;
    a_ranks.sort_unstable();
    assert_eq!(
        a_ranks,
        vec![0, 1, 2, 3],
        "board A must compact to a contiguous 0..3 permutation after the row left: {a_ranks:?}"
    );

    let mut b_ranks = triggered_ranks(&db.pool(), board_b).await;
    b_ranks.sort_unstable();
    assert_eq!(
        b_ranks,
        vec![0, 1, 2, 3],
        "board B must have the moved row appended at the end (no duplicate rank): {b_ranks:?}"
    );
    let moved_rank: i64 = position_triggered_tasks::table
        .find(moved_id)
        .select(position_triggered_tasks::rank)
        .first(&mut conn)
        .await
        .expect("load moved row's rank");
    assert_eq!(
        moved_rank, 3,
        "the moved row must land at the END of board B's sequence, not overwrite an \
         existing rank"
    );
}
