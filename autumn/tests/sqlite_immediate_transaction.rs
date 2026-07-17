//! `BEGIN IMMEDIATE` write-transaction proof on the `SQLite` runtime backend
//! (issue #1996, item 3).
//!
//! The generated write read-modify-write paths (`with_lock`, `update`,
//! `delete_by_id`, `find_or_create_by`) route through
//! [`autumn_web::db::scoped_immediate_transaction`], whose `SQLite` arm issues
//! `BEGIN IMMEDIATE` so a concurrent writer takes the write lock up front and a
//! second writer *queues* on the connection `busy_timeout` instead of failing
//! its deferred read→write snapshot upgrade with `SQLITE_BUSY_SNAPSHOT` (which
//! bypasses the busy handler). These tests prove that end to end on a real
//! shared tempfile database (not `:memory:`, which is private per connection):
//!
//! 1. two concurrent write-RMW writers on the same row serialize with no lost
//!    update and no busy error;
//! 2. an `update` that races a held write lock queues and then commits (and
//!    bumps the optimistic `lock_version`);
//! 3. a single-writer `update` commits and bumps `lock_version`;
//! 4. a write-RMW closure that returns `Err` rolls the immediate transaction
//!    back (the row is unchanged).
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_immediate_transaction`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::hooks::Patch;
use autumn_web::reexports::{diesel, diesel_async, scoped_futures};

// Only the query-builder traits (`.find()` / `.eq()`); the executing
// `RunQueryDsl` must be the async one, so avoid `diesel::prelude::*` (it would
// also pull in the sync `diesel::RunQueryDsl` and make `.execute()` ambiguous).
use diesel::{ExpressionMethods as _, QueryDsl as _};
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;
use scoped_futures::ScopedFutureExt as _;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        counters (id) {
            id -> Int8,
            value -> Int8,
            lock_version -> Int8,
        }
    }
}

use schema::counters;

// A plain optimistic-lock (`#[lock_version]`) model — no tenant, no version
// history, no hooks — so the generated CRUD compiles on the SQLite backend.
#[autumn_web::model]
pub struct Counter {
    #[id]
    pub id: i64,
    pub value: i64,
    #[lock_version]
    pub lock_version: i64,
}

#[autumn_web::repository(Counter)]
pub trait CounterRepository {}

async fn boot_pool(db_path: &std::path::Path) -> SqlitePool {
    // A tempfile-backed database (WAL, `busy_timeout` — installed by the pool's
    // per-connection setup) so both pooled connections observe one shared file
    // and the write lock is a real cross-connection lock, unlike a private
    // `:memory:` handle.
    let url = format!("sqlite://{}", db_path.display());
    let config = DatabaseConfig {
        url: Some(url),
        // ≥ 2 slots so the two concurrent writers get distinct connections.
        primary_pool_size: Some(4),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query(
            "CREATE TABLE counters (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 value BIGINT NOT NULL DEFAULT 0, \
                 lock_version BIGINT NOT NULL DEFAULT 1\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create counters table");
    }

    pool
}

// with_lock (a pessimistic write RMW → BEGIN IMMEDIATE) that optionally holds
// the write lock for `hold_ms` before applying `+delta` to `value`.
async fn locked_increment(
    repo: &PgCounterRepository,
    id: i64,
    delta: i64,
    hold_ms: u64,
) -> autumn_web::AutumnResult<()> {
    repo.with_lock(id, move |record, conn| {
        async move {
            if hold_ms > 0 {
                // Held while BEGIN IMMEDIATE owns the write lock, so a concurrent
                // writer must queue on busy_timeout.
                tokio::time::sleep(std::time::Duration::from_millis(hold_ms)).await;
            }
            diesel::update(counters::table.find(record.id))
                .set(counters::value.eq(record.value + delta))
                .execute(conn)
                .await
                .map_err(autumn_web::AutumnError::from)?;
            Ok::<(), autumn_web::AutumnError>(())
        }
        .scope_boxed()
    })
    .await
}

#[tokio::test]
async fn concurrent_immediate_rmw_writers_serialize_without_lost_update() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = boot_pool(&tmp.path().join("serialize.db")).await;
    let repo = PgCounterRepository::with_pool_untracked(pool);

    let base = repo.save(&NewCounter { value: 0 }).await.expect("seed row");

    // Writer A holds the immediate write lock for 150ms, then +10.
    // Writer B starts ~40ms later so A already owns the lock; its own
    // BEGIN IMMEDIATE queues on busy_timeout and runs +5 once A commits.
    let a = locked_increment(&repo, base.id, 10, 150);
    let b = async {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        locked_increment(&repo, base.id, 5, 0).await
    };
    let (ra, rb) = tokio::join!(a, b);
    ra.expect("writer A commits");
    rb.expect("writer B queues on busy_timeout and commits (no SQLITE_BUSY_SNAPSHOT)");

    let final_row = repo
        .find_by_id(base.id)
        .await
        .expect("find")
        .expect("row exists");
    // Both increments landed on top of A's committed value → no lost update.
    assert_eq!(
        final_row.value, 15,
        "both writers' increments are reflected"
    );
}

#[tokio::test]
async fn update_queued_behind_held_write_lock_commits() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = boot_pool(&tmp.path().join("queued.db")).await;
    let repo = PgCounterRepository::with_pool_untracked(pool);

    let base = repo.save(&NewCounter { value: 1 }).await.expect("seed row");

    // A holds the write lock 150ms; B is the generated `update` RMW (also
    // BEGIN IMMEDIATE) — it must queue and then succeed, never erroring.
    let a = locked_increment(&repo, base.id, 100, 150);
    let b = async {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        repo.update(
            base.id,
            &UpdateCounter {
                value: Patch::Set(999),
                // A's with_lock did not bump lock_version, so the row is still at
                // the seeded version when B's queued update finally runs.
                lock_version: base.lock_version,
            },
        )
        .await
    };
    let (ra, rb) = tokio::join!(a, b);
    ra.expect("writer A commits");
    let updated = rb.expect("update queues on busy_timeout and commits");

    // B ran strictly after A committed (immediate lock serialized them), and the
    // optimistic lock_version was bumped by the update.
    assert_eq!(updated.value, 999);
    assert_eq!(updated.lock_version, 2, "update bumps lock_version");
    let final_row = repo
        .find_by_id(base.id)
        .await
        .expect("find")
        .expect("row exists");
    assert_eq!(final_row.value, 999);
    assert_eq!(final_row.lock_version, 2);
}

#[tokio::test]
async fn single_writer_update_commits_and_bumps_lock_version() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = boot_pool(&tmp.path().join("single.db")).await;
    let repo = PgCounterRepository::with_pool_untracked(pool);

    let base = repo.save(&NewCounter { value: 7 }).await.expect("seed row");

    let updated = repo
        .update(
            base.id,
            &UpdateCounter {
                value: Patch::Set(42),
                lock_version: base.lock_version,
            },
        )
        .await
        .expect("single-writer immediate update commits");
    assert_eq!(updated.value, 42);
    assert_eq!(updated.lock_version, 2);
}

#[tokio::test]
async fn errored_rmw_rolls_back_the_immediate_transaction() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let pool = boot_pool(&tmp.path().join("rollback.db")).await;
    let repo = PgCounterRepository::with_pool_untracked(pool);

    let base = repo.save(&NewCounter { value: 5 }).await.expect("seed row");

    // Mutate inside the immediate transaction, then return Err → the SQLite arm
    // must ROLLBACK, leaving the row untouched.
    let outcome: autumn_web::AutumnResult<()> = repo
        .with_lock(base.id, |record, conn| {
            async move {
                diesel::update(counters::table.find(record.id))
                    .set(counters::value.eq(record.value + 1000))
                    .execute(conn)
                    .await
                    .map_err(autumn_web::AutumnError::from)?;
                Err(autumn_web::AutumnError::internal_server_error_msg(
                    "intentional rollback",
                ))
            }
            .scope_boxed()
        })
        .await;
    assert!(outcome.is_err(), "the RMW closure returned Err");

    let after = repo
        .find_by_id(base.id)
        .await
        .expect("find")
        .expect("row exists");
    assert_eq!(after.value, 5, "the mutation was rolled back");
    assert_eq!(after.lock_version, 1, "no lock_version bump on rollback");
}
