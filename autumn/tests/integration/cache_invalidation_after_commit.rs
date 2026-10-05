//! Commit-bound cache invalidation (#3056).
//!
//! A repository with declared `invalidates(...)` edges must drop those cached
//! reads after each write commits. The app does not call the invalidator.
//!
//! The race this closes: a reader reads the row before the commit, and puts
//! that old value in the cache after the invalidation. The write invalidates
//! after its own commit, and the epoch fence stops the late insert.

#![cfg(all(feature = "db", feature = "cache-moka", feature = "test-support"))]

use std::sync::LazyLock;

use autumn_web::AutumnResult;
use autumn_web::test::TestDb;
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use tokio::sync::Notify;

mod schema {
    autumn_web::reexports::diesel::table! {
        after_commit_notes (id) {
            id -> Int8,
            label -> Text,
        }
    }
}

use schema::after_commit_notes;

#[autumn_web::model(table = "after_commit_notes")]
pub struct AfterCommitNote {
    #[id]
    pub id: i64,
    pub label: String,
}

/// How many notes carry `label`. Cached, keyed by the label only. An error is
/// not cached (`result`), so a failed read cannot look like a stale value.
#[autumn_web::cached(key(label), reads(AfterCommitNote), result)]
pub async fn after_commit_note_count(
    label: String,
    repo: &PgAfterCommitNoteRepository,
) -> AutumnResult<usize> {
    Ok(repo.find_by_label(label).await?.len())
}

/// Serializes this module's tests. Every write here invalidates the whole
/// namespace, so a write in one test could clear a value that another test
/// expects its own write to clear, and hide a missing invalidation.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Signals that the slow reader has read the database.
static SLOW_READ_DONE: LazyLock<Notify> = LazyLock::new(Notify::new);
/// Lets the slow reader return, and so insert into the cache.
static SLOW_READ_RELEASE: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Like [`after_commit_note_count`], but it stops between the database read
/// and the cache insert until the test releases it.
#[autumn_web::cached(key(label), reads(AfterCommitNote), result)]
pub async fn after_commit_slow_note_count(
    label: String,
    repo: &PgAfterCommitNoteRepository,
) -> AutumnResult<usize> {
    let count = repo.find_by_label(label).await?.len();
    SLOW_READ_DONE.notify_one();
    SLOW_READ_RELEASE.notified().await;
    Ok(count)
}

#[autumn_web::repository(
    AfterCommitNote,
    table = "after_commit_notes",
    invalidates(after_commit_note_count, after_commit_slow_note_count)
)]
pub trait AfterCommitNoteRepository {
    fn find_by_label(label: String) -> Vec<AfterCommitNote>;
    fn delete_by_label(label: String) -> ();
}

/// A repository whose `after_create` hook fails after the row commits.
#[derive(Clone, Default)]
pub struct FailingAfterCreateHooks;

impl autumn_web::hooks::MutationHooks for FailingAfterCreateHooks {
    type Model = AfterCommitNote;
    type NewModel = NewAfterCommitNote;
    type UpdateModel = UpdateAfterCommitNote;

    async fn after_create(
        &self,
        _ctx: &mut autumn_web::hooks::MutationContext,
        _record: &AfterCommitNote,
    ) -> autumn_web::AutumnResult<()> {
        Err(autumn_web::AutumnError::internal_server_error_msg(
            "after_create fails after the commit",
        ))
    }
}

#[autumn_web::repository(
    AfterCommitNote,
    table = "after_commit_notes",
    hooks = FailingAfterCreateHooks,
    invalidates(after_commit_note_count)
)]
pub trait FailingHookNoteRepository {
    fn delete_by_label(label: String) -> ();
}

/// Signals that a `save` is inside its transaction, before the commit.
static IN_TX_ENTERED: LazyLock<Notify> = LazyLock::new(Notify::new);
/// Lets that `save` go on to insert and commit.
static IN_TX_RELEASE: LazyLock<Notify> = LazyLock::new(Notify::new);

/// A repository whose `before_create` hook waits inside the transaction.
#[derive(Clone, Default)]
pub struct GatedBeforeCreateHooks;

impl autumn_web::hooks::MutationHooks for GatedBeforeCreateHooks {
    type Model = AfterCommitNote;
    type NewModel = NewAfterCommitNote;
    type UpdateModel = UpdateAfterCommitNote;

    async fn before_create(
        &self,
        _ctx: &mut autumn_web::hooks::MutationContext,
        _new: &mut NewAfterCommitNote,
    ) -> autumn_web::AutumnResult<()> {
        IN_TX_ENTERED.notify_one();
        IN_TX_RELEASE.notified().await;
        Ok(())
    }
}

#[autumn_web::repository(
    AfterCommitNote,
    table = "after_commit_notes",
    hooks = GatedBeforeCreateHooks,
    invalidates(after_commit_note_count)
)]
pub trait GatedNoteRepository {}

// ── The retention sweep (Codex review on #3137) ─────────────────────

mod aged_schema {
    autumn_web::reexports::diesel::table! {
        after_commit_aged (id) {
            id -> Int8,
            created_at -> Timestamp,
        }
    }
}

use aged_schema::after_commit_aged;

#[autumn_web::model(table = "after_commit_aged")]
pub struct AfterCommitAged {
    #[id]
    pub id: i64,
    pub created_at: chrono::NaiveDateTime,
}

/// How many aged rows exist. Cached, keyed by `scope` only.
#[autumn_web::cached(key(scope), reads(AfterCommitAged), result)]
pub async fn after_commit_aged_count(
    scope: u8,
    repo: &PgAfterCommitAgedRepository,
) -> AutumnResult<usize> {
    let _ = scope;
    Ok(repo.find_all().await?.len())
}

#[autumn_web::repository(
    AfterCommitAged,
    table = "after_commit_aged",
    invalidates(after_commit_aged_count),
    retention(after = "30d", basis = created_at)
)]
pub trait AfterCommitAgedRepository {}

/// Creates the table once. Two concurrent `CREATE TABLE IF NOT EXISTS` can
/// still collide in Postgres.
static TABLE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// A pool for this test only. Each `#[tokio::test]` has its own runtime, and a
/// connection dies with the runtime that opened it, so tests must not share
/// `TestDb::pool()`.
async fn pool() -> Pool<AsyncPgConnection> {
    let db = TestDb::shared().await;
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(db.url());
    let pool = Pool::builder(manager).max_size(4).build().expect("pool");
    TABLE
        .get_or_init(|| async {
            let mut conn = pool.get().await.expect("db connection");
            diesel::sql_query(
                "CREATE TABLE IF NOT EXISTS after_commit_notes (
                    id BIGSERIAL PRIMARY KEY,
                    label TEXT NOT NULL
                )",
            )
            .execute(&mut *conn)
            .await
            .expect("create after_commit_notes");
        })
        .await;
    pool
}

async fn repository() -> PgAfterCommitNoteRepository {
    PgAfterCommitNoteRepository::with_pool_untracked(pool().await)
}

fn note(label: &str) -> NewAfterCommitNote {
    NewAfterCommitNote {
        label: label.to_owned(),
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn each_generated_write_invalidates_after_commit_with_no_manual_call() {
    let _serial = SERIAL.lock().await;
    let repo = repository().await;
    let label = "each-write".to_owned();

    assert_eq!(
        after_commit_note_count(label.clone(), &repo)
            .await
            .expect("count"),
        0
    );

    let saved = repo.save(&note(&label)).await.expect("save");
    assert_eq!(
        after_commit_note_count(label.clone(), &repo)
            .await
            .expect("count"),
        1,
        "save must drop the cached 0"
    );

    repo.save_many(&[note(&label), note(&label)])
        .await
        .expect("save_many");
    assert_eq!(
        after_commit_note_count(label.clone(), &repo)
            .await
            .expect("count"),
        3
    );

    repo.update(
        saved.id,
        &UpdateAfterCommitNote {
            label: autumn_web::Patch::Set("moved-away".to_owned()),
        },
    )
    .await
    .expect("update");
    assert_eq!(
        after_commit_note_count(label.clone(), &repo)
            .await
            .expect("count"),
        2
    );

    repo.delete_by_label(label.clone())
        .await
        .expect("delete_by_label");
    assert_eq!(
        after_commit_note_count(label.clone(), &repo)
            .await
            .expect("count"),
        0,
        "a derived delete_by_* write must invalidate too"
    );

    repo.delete_by_id(saved.id).await.expect("delete_by_id");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker (testcontainers)"]
async fn a_reader_that_read_before_the_commit_cannot_repopulate_the_old_value() {
    let _serial = SERIAL.lock().await;
    let repo = repository().await;
    let label = "pre-commit-reader".to_owned();

    // The reader reads the table (no rows yet), then waits before its insert.
    let reader = {
        let (repo, label) = (repo.clone(), label.clone());
        tokio::spawn(async move {
            after_commit_slow_note_count(label, &repo)
                .await
                .expect("count")
        })
    };
    SLOW_READ_DONE.notified().await;

    // The write commits and invalidates while the reader holds the old count.
    repo.save(&note(&label)).await.expect("save");

    // The reader now tries to put the old count in the cache.
    SLOW_READ_RELEASE.notify_one();
    assert_eq!(
        reader.await.expect("reader task"),
        0,
        "the reader returns what it read"
    );

    // The next read must see the committed row, not the reader's old count.
    // Release first: a cache hit never runs the body, so it must not block.
    SLOW_READ_RELEASE.notify_one();
    assert_eq!(
        after_commit_slow_note_count(label.clone(), &repo)
            .await
            .expect("count"),
        1,
        "the pre-commit value must not be in the cache after the commit"
    );

    repo.delete_by_label(label).await.expect("clean up");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn a_write_that_commits_then_fails_still_invalidates() {
    let _serial = SERIAL.lock().await;
    let repo = repository().await;
    let failing = PgFailingHookNoteRepository::with_pool_untracked(pool().await);
    let label = "commit-then-fail".to_owned();

    assert_eq!(
        after_commit_note_count(label.clone(), &repo)
            .await
            .expect("count"),
        0
    );

    let result = failing.save(&note(&label)).await;
    assert!(result.is_err(), "the after_create hook fails the call");
    assert_eq!(
        after_commit_note_count(label.clone(), &repo)
            .await
            .expect("count"),
        1,
        "the row committed, so the cached 0 must be gone"
    );

    repo.delete_by_label(label).await.expect("clean up");
}

/// The interleaving that breaks invalidate-then-commit: a reader runs to
/// completion while the write is inside its transaction. It reads the old
/// state and caches it. Only an invalidation after the commit removes it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker (testcontainers)"]
async fn a_reader_inside_the_transaction_window_cannot_keep_the_old_value() {
    let _serial = SERIAL.lock().await;
    let repo = repository().await;
    let gated = PgGatedNoteRepository::with_pool_untracked(pool().await);
    let label = "in-tx-window".to_owned();

    let writer = {
        let (gated, label) = (gated.clone(), label.clone());
        tokio::spawn(async move { gated.save(&note(&label)).await })
    };
    IN_TX_ENTERED.notified().await;

    // The write is open and not committed. This read caches the old count.
    assert_eq!(
        after_commit_note_count(label.clone(), &repo)
            .await
            .expect("count"),
        0
    );

    IN_TX_RELEASE.notify_one();
    writer.await.expect("writer task").expect("save");

    assert_eq!(
        after_commit_note_count(label.clone(), &repo)
            .await
            .expect("count"),
        1,
        "the value cached before the commit must be gone after the commit"
    );

    repo.delete_by_label(label).await.expect("clean up");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn the_retention_sweep_invalidates_after_it_deletes_rows() {
    let _serial = SERIAL.lock().await;
    let pool = pool().await;
    let mut conn = pool.get().await.expect("db connection");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS after_commit_aged (
            id BIGSERIAL PRIMARY KEY,
            created_at TIMESTAMP NOT NULL
        )",
    )
    .execute(&mut *conn)
    .await
    .expect("create after_commit_aged");
    diesel::sql_query("DELETE FROM after_commit_aged")
        .execute(&mut *conn)
        .await
        .expect("empty after_commit_aged");
    diesel::sql_query(
        "INSERT INTO after_commit_aged (created_at) VALUES (NOW() - INTERVAL '90 days')",
    )
    .execute(&mut *conn)
    .await
    .expect("seed an expired row");
    drop(conn);

    let repo = PgAfterCommitAgedRepository::with_pool_untracked(pool.clone());
    assert_eq!(after_commit_aged_count(0, &repo).await.expect("count"), 1);

    let state = autumn_web::AppState::for_test().with_pool(pool);
    let report = PgAfterCommitAgedRepository::__autumn_retention_sweep(&state)
        .await
        .expect("sweep");
    assert_eq!(report.rows_swept, 1, "the expired row is swept");

    assert_eq!(
        after_commit_aged_count(0, &repo).await.expect("count"),
        0,
        "the sweep committed a delete, so the cached 1 must be gone"
    );
}
