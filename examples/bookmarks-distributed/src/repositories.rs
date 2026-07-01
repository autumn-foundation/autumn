use std::sync::Arc;

use autumn_web::extract::Path;
use autumn_web::prelude::*;
use diesel::OptionalExtension;
use diesel::QueryableByName;
use diesel::prelude::*;
use diesel::result::{Error as DieselError, QueryResult};
use diesel::sql_types::{BigInt, Bool, Integer, Text};
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;
use diesel_async::pooled_connection::deadpool::{Object, Pool};

use crate::models::{Bookmark, NewBookmark, UpdateBookmark};
use crate::schema::bookmarks;
use crate::state::DistributedState;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BookmarkRepository;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolRole {
    Primary,
    Replica,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookmarkOperation {
    FindAll,
    FindByTag,
    FindById,
    FindAliveInShard,
    Save,
    Update,
    DeleteById,
    MarkDead,
    /// Acquiring the link checker's per-shard advisory lock — needs a
    /// primary connection like a write does, but isn't itself a write, so
    /// it's kept out of `conn()`'s `mark_write()` set.
    AcquireShardLease,
}

pub(crate) const LINK_CHECKER_SHARD_COUNT: u32 = 16;
const LINK_CHECKER_LOCK_NAMESPACE: i32 = 0x4155_4C4B;

pub(crate) struct ShardLease {
    conn: AsyncPgConnection,
    shard: u32,
}

#[derive(Debug, QueryableByName)]
struct LockResult {
    #[diesel(sql_type = Bool)]
    acquired: bool,
}

#[derive(Debug, QueryableByName)]
struct UnlockResult {
    #[diesel(sql_type = Bool)]
    released: bool,
}

#[derive(Debug, QueryableByName)]
struct AliveBookmarkRow {
    #[diesel(sql_type = BigInt)]
    id: i64,
    #[diesel(sql_type = Text)]
    url: String,
}

impl BookmarkRepository {
    #[must_use]
    pub const fn role_for(operation: BookmarkOperation) -> PoolRole {
        match operation {
            BookmarkOperation::FindAll
            | BookmarkOperation::FindByTag
            | BookmarkOperation::FindById
            | BookmarkOperation::FindAliveInShard => PoolRole::Replica,
            BookmarkOperation::Save
            | BookmarkOperation::Update
            | BookmarkOperation::DeleteById
            | BookmarkOperation::MarkDead
            | BookmarkOperation::AcquireShardLease => PoolRole::Primary,
        }
    }

    #[must_use]
    pub(crate) fn shard_ids() -> std::ops::Range<u32> {
        0..LINK_CHECKER_SHARD_COUNT
    }

    #[must_use]
    pub(crate) fn link_checker_lock_key(shard: u32) -> (i32, i32) {
        (
            LINK_CHECKER_LOCK_NAMESPACE,
            i32::try_from(shard).expect("shard id must fit in i32"),
        )
    }

    fn distributed_state() -> AutumnResult<Arc<DistributedState>> {
        DistributedState::global().ok_or_else(|| {
            AutumnError::service_unavailable_msg("distributed state is not installed")
        })
    }

    fn pool(state: &DistributedState, role: PoolRole) -> &Pool<AsyncPgConnection> {
        match role {
            PoolRole::Primary => state.pools.primary(),
            PoolRole::Replica => state.pools.replica(),
        }
    }

    /// Resolve the pool `operation` should actually use, honoring
    /// `database.read_your_writes`.
    ///
    /// This repository predates the `#[repository(...)]` macro (it hand-rolls
    /// pool selection for finer-grained shard-lease control), so it doesn't
    /// get RYWW's generated-code wiring for free — it opts in explicitly via
    /// the same public `is_pinned` call the macro emits. A replica-eligible
    /// read is redirected to primary while the current request (or session,
    /// depending on `read_your_writes` mode) is pinned; `role_for`'s answer
    /// is otherwise final. Split out from [`Self::conn`] so this decision is
    /// testable without a live pool.
    fn effective_role(operation: BookmarkOperation) -> PoolRole {
        let role = Self::role_for(operation);
        if role == PoolRole::Replica && autumn_web::read_your_writes::is_pinned() {
            autumn_web::read_your_writes::note_pin_redirect();
            PoolRole::Primary
        } else {
            role
        }
    }

    /// Whether `operation` is a genuine write that should call `mark_write()`.
    ///
    /// `AcquireShardLease` also targets the primary (see [`Self::role_for`])
    /// but taking an advisory lock isn't a write, so it's deliberately
    /// excluded — split out from [`Self::conn`] so that's testable without a
    /// live pool.
    const fn is_write(operation: BookmarkOperation) -> bool {
        matches!(
            operation,
            BookmarkOperation::Save
                | BookmarkOperation::Update
                | BookmarkOperation::DeleteById
                | BookmarkOperation::MarkDead
        )
    }

    /// Acquire a connection for `operation` from the pool [`Self::effective_role`]
    /// selects. Only genuine writes call `mark_write()` — mirroring the
    /// macro's "mark only after a successful primary acquire" behavior —
    /// not every pin-redirected read.
    async fn conn(
        operation: BookmarkOperation,
    ) -> AutumnResult<diesel_async::pooled_connection::deadpool::Object<AsyncPgConnection>> {
        let role = Self::effective_role(operation);
        let state = Self::distributed_state()?;
        let pool = Self::pool(&state, role);
        let conn = pool.get().await.map_err(AutumnError::from)?;
        if Self::is_write(operation) {
            autumn_web::read_your_writes::mark_write();
        }
        Ok(conn)
    }

    fn missing_bookmark_error(operation: &'static str, id: i64) -> AutumnError {
        AutumnError::not_found_msg(format!(
            "bookmark with id {id} not found during {operation}"
        ))
    }

    fn finish_update_result(result: QueryResult<Bookmark>, id: i64) -> AutumnResult<Bookmark> {
        result.map_err(|error| match error {
            DieselError::NotFound => Self::missing_bookmark_error("update", id),
            other => AutumnError::from(other),
        })
    }

    fn finish_mark_dead_result(affected: usize, id: i64) -> AutumnResult<bool> {
        if affected == 0 {
            // Replica lag or a concurrent delete can make this row disappear after the
            // task observed it alive. Treat that as a benign no-op so one stale row does
            // not abort the whole task run.
            tracing::debug!(
                bookmark_id = id,
                "link-checker skipped stale dead-link update"
            );
            return Ok(false);
        }
        Ok(true)
    }

    async fn try_acquire_shard_lease(shard: u32) -> AutumnResult<Option<ShardLease>> {
        let mut conn = Self::conn(BookmarkOperation::AcquireShardLease).await?;
        let (namespace, shard_key) = Self::link_checker_lock_key(shard);
        let result = diesel::sql_query("SELECT pg_try_advisory_lock($1, $2) AS acquired")
            .bind::<Integer, _>(namespace)
            .bind::<Integer, _>(shard_key)
            .get_result::<LockResult>(&mut conn)
            .await
            .map_err(AutumnError::from)?;

        if result.acquired {
            Ok(Some(ShardLease {
                conn: Object::take(conn),
                shard,
            }))
        } else {
            Ok(None)
        }
    }

    async fn unlock_shard_lease(lease: ShardLease) -> AutumnResult<()> {
        let ShardLease { mut conn, shard } = lease;
        let (namespace, shard_key) = Self::link_checker_lock_key(shard);
        let result = diesel::sql_query("SELECT pg_advisory_unlock($1, $2) AS released")
            .bind::<Integer, _>(namespace)
            .bind::<Integer, _>(shard_key)
            .get_result::<UnlockResult>(&mut conn)
            .await;

        match result {
            Ok(result) if result.released => Ok(()),
            Ok(_) | Err(_) => {
                // Advisory locks are session-scoped. By the time we get here, the lease is
                // already detached from the pool, so dropping the raw connection closes the
                // session instead of recycling a lock-bearing object.
                drop(conn);
                Err(AutumnError::service_unavailable_msg(format!(
                    "failed to release advisory lock for shard {shard_key}"
                )))
            }
        }
    }

    pub async fn find_all(&self) -> AutumnResult<Vec<Bookmark>> {
        let mut conn = Self::conn(BookmarkOperation::FindAll).await?;
        bookmarks::table
            .load::<Bookmark>(&mut conn)
            .await
            .map_err(AutumnError::from)
    }

    pub async fn find_alive_in_shard(&self, shard: u32) -> AutumnResult<Vec<(i64, String)>> {
        let mut conn = Self::conn(BookmarkOperation::FindAliveInShard).await?;
        diesel::sql_query(
            "SELECT id, url FROM bookmarks WHERE alive = true AND (id % $1) = $2 ORDER BY id",
        )
        .bind::<BigInt, _>(i64::from(LINK_CHECKER_SHARD_COUNT))
        .bind::<BigInt, _>(i64::from(shard))
        .load::<AliveBookmarkRow>(&mut conn)
        .await
        .map(|rows| rows.into_iter().map(|row| (row.id, row.url)).collect())
        .map_err(AutumnError::from)
    }

    pub async fn find_by_tag(&self, tag: String) -> AutumnResult<Vec<Bookmark>> {
        let mut conn = Self::conn(BookmarkOperation::FindByTag).await?;
        bookmarks::table
            .filter(bookmarks::tag.eq(tag))
            .load::<Bookmark>(&mut conn)
            .await
            .map_err(AutumnError::from)
    }

    pub async fn find_by_id(&self, id: i64) -> AutumnResult<Option<Bookmark>> {
        let mut conn = Self::conn(BookmarkOperation::FindById).await?;
        bookmarks::table
            .find(id)
            .first::<Bookmark>(&mut conn)
            .await
            .optional()
            .map_err(AutumnError::from)
    }

    pub async fn save(&self, new: &NewBookmark) -> AutumnResult<Bookmark> {
        let mut conn = Self::conn(BookmarkOperation::Save).await?;
        diesel::insert_into(bookmarks::table)
            .values(new)
            .get_result::<Bookmark>(&mut conn)
            .await
            .map_err(AutumnError::from)
    }

    pub async fn update(&self, id: i64, changes: &UpdateBookmark) -> AutumnResult<Bookmark> {
        let mut conn = Self::conn(BookmarkOperation::Update).await?;
        let changeset = changes.__to_changeset();
        let result = diesel::update(bookmarks::table.find(id))
            .set(&changeset)
            .get_result::<Bookmark>(&mut conn)
            .await;
        Self::finish_update_result(result, id)
    }

    pub async fn delete_by_id(&self, id: i64) -> AutumnResult<()> {
        let mut conn = Self::conn(BookmarkOperation::DeleteById).await?;
        let affected = diesel::delete(bookmarks::table.find(id))
            .execute(&mut conn)
            .await
            .map_err(AutumnError::from)?;
        if affected == 0 {
            return Err(Self::missing_bookmark_error("delete", id));
        }
        Ok(())
    }

    pub async fn mark_dead(&self, id: i64) -> AutumnResult<bool> {
        let mut conn = Self::conn(BookmarkOperation::MarkDead).await?;
        let affected = diesel::update(bookmarks::table.find(id))
            .set(bookmarks::alive.eq(false))
            .execute(&mut conn)
            .await
            .map_err(AutumnError::from)?;
        Self::finish_mark_dead_result(affected, id)
    }

    pub(crate) async fn acquire_shard_lease(shard: u32) -> AutumnResult<Option<ShardLease>> {
        Self::try_acquire_shard_lease(shard).await
    }

    pub(crate) async fn release_shard_lease(lease: ShardLease) -> AutumnResult<()> {
        Self::unlock_shard_lease(lease).await
    }

    pub async fn count_all(&self) -> AutumnResult<i64> {
        let mut conn = Self::conn(BookmarkOperation::FindAll).await?;
        bookmarks::table
            .count()
            .get_result::<i64>(&mut conn)
            .await
            .map_err(AutumnError::from)
    }
}

/// Returns the total number of bookmarks, cached for 30 s across all replicas.
///
/// When `RedisCachePlugin` is active (docker profile) this count is shared
/// across all replicas so only one DB round-trip happens per 30-second window,
/// regardless of which replica receives the request.
#[cached(ttl = "30s", result)]
pub async fn cached_bookmark_count() -> AutumnResult<i64> {
    BookmarkRepository.count_all().await
}

#[get("/api/bookmarks/count")]
pub async fn bookmark_api_count() -> AutumnResult<Json<i64>> {
    Ok(Json(cached_bookmark_count().await?))
}

#[get("/api/bookmarks")]
pub async fn bookmark_api_list() -> AutumnResult<Json<Vec<Bookmark>>> {
    let repo = BookmarkRepository;
    Ok(Json(repo.find_all().await?))
}

#[get("/api/bookmarks/{id}")]
pub async fn bookmark_api_get(Path(id): Path<i64>) -> AutumnResult<Json<Bookmark>> {
    let repo = BookmarkRepository;
    let record = repo
        .find_by_id(id)
        .await?
        .ok_or_else(|| AutumnError::not_found_msg(format!("bookmark with id {id} not found")))?;
    Ok(Json(record))
}

#[post("/api/bookmarks")]
pub async fn bookmark_api_create(
    Json(new): Json<NewBookmark>,
) -> AutumnResult<(autumn_web::reexports::http::StatusCode, Json<Bookmark>)> {
    let repo = BookmarkRepository;
    let record = repo.save(&new).await?;
    Ok((
        autumn_web::reexports::http::StatusCode::CREATED,
        Json(record),
    ))
}

#[put("/api/bookmarks/{id}")]
pub async fn bookmark_api_update(
    Path(id): Path<i64>,
    Json(changes): Json<UpdateBookmark>,
) -> AutumnResult<Json<Bookmark>> {
    let repo = BookmarkRepository;
    Ok(Json(repo.update(id, &changes).await?))
}

#[delete("/api/bookmarks/{id}")]
pub async fn bookmark_api_delete(
    Path(id): Path<i64>,
) -> AutumnResult<autumn_web::reexports::http::StatusCode> {
    let repo = BookmarkRepository;
    repo.delete_by_id(id).await?;
    Ok(autumn_web::reexports::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::{BookmarkOperation, BookmarkRepository, LINK_CHECKER_SHARD_COUNT, PoolRole};
    use crate::config::DistributedConfig;
    use crate::db::create_dual_pools;
    use crate::state::DistributedState;
    use autumn_web::config::ReadYourWrites;
    use autumn_web::read_your_writes::{RequestPin, mark_write, scope};
    use autumn_web::test::TestDb;
    use diesel::result::Error as DieselError;
    use std::sync::Arc;

    #[test]
    fn read_operations_use_replica_and_writes_use_primary() {
        assert_eq!(
            BookmarkRepository::role_for(BookmarkOperation::FindAll),
            PoolRole::Replica
        );
        assert_eq!(
            BookmarkRepository::role_for(BookmarkOperation::FindByTag),
            PoolRole::Replica
        );
        assert_eq!(
            BookmarkRepository::role_for(BookmarkOperation::FindById),
            PoolRole::Replica
        );
        assert_eq!(
            BookmarkRepository::role_for(BookmarkOperation::FindAliveInShard),
            PoolRole::Replica
        );
        assert_eq!(
            BookmarkRepository::role_for(BookmarkOperation::Save),
            PoolRole::Primary
        );
        assert_eq!(
            BookmarkRepository::role_for(BookmarkOperation::Update),
            PoolRole::Primary
        );
        assert_eq!(
            BookmarkRepository::role_for(BookmarkOperation::DeleteById),
            PoolRole::Primary
        );
        assert_eq!(
            BookmarkRepository::role_for(BookmarkOperation::MarkDead),
            PoolRole::Primary
        );
        assert_eq!(
            BookmarkRepository::role_for(BookmarkOperation::AcquireShardLease),
            PoolRole::Primary
        );
    }

    #[test]
    fn acquire_shard_lease_targets_primary_but_is_not_a_write() {
        assert!(
            !BookmarkRepository::is_write(BookmarkOperation::AcquireShardLease),
            "taking an advisory lock must not call mark_write(), even though it \
             needs a primary connection like a write does"
        );
    }

    #[test]
    fn writes_are_all_marked() {
        for op in [
            BookmarkOperation::Save,
            BookmarkOperation::Update,
            BookmarkOperation::DeleteById,
            BookmarkOperation::MarkDead,
        ] {
            assert!(BookmarkRepository::is_write(op), "{op:?} must mark a write");
        }
    }

    // ── RYWW wiring (BookmarkRepository::effective_role) ────────────────────
    //
    // This repository predates the `#[repository(...)]` macro, so it doesn't
    // get RYWW's pin-checking for free — it opts in explicitly in `conn()`.
    // These tests exercise that opt-in against the same public `is_pinned`
    // task-local the framework's own generated code and RYW middleware use
    // (see `autumn/src/read_your_writes.rs`), without needing a live pool —
    // the full write-then-read path is covered by
    // `tests/system/smoke.rs::bookmarks_distributed_read_your_own_write_after_create`.

    #[tokio::test]
    async fn effective_role_replica_reads_stay_on_replica_with_no_pin() {
        assert_eq!(
            BookmarkRepository::effective_role(BookmarkOperation::FindAll),
            PoolRole::Replica,
            "outside any RYWW scope, reads must keep going to the replica"
        );
    }

    #[tokio::test]
    async fn effective_role_replica_reads_stay_on_replica_before_a_write() {
        let pin = RequestPin::new(ReadYourWrites::Request);
        scope(pin, async {
            assert_eq!(
                BookmarkRepository::effective_role(BookmarkOperation::FindAll),
                PoolRole::Replica,
                "a pin scope alone (no write yet) must not redirect reads"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn effective_role_redirects_replica_reads_to_primary_after_a_write() {
        let pin = RequestPin::new(ReadYourWrites::Request);
        scope(pin, async {
            mark_write();
            assert_eq!(
                BookmarkRepository::effective_role(BookmarkOperation::FindAll),
                PoolRole::Primary,
                "a replica-eligible read after a write must redirect to primary"
            );
            assert_eq!(
                BookmarkRepository::effective_role(BookmarkOperation::FindByTag),
                PoolRole::Primary
            );
        })
        .await;
    }

    #[tokio::test]
    async fn effective_role_ignores_the_pin_when_read_your_writes_is_off() {
        let pin = RequestPin::new(ReadYourWrites::Off);
        scope(pin, async {
            mark_write();
            assert_eq!(
                BookmarkRepository::effective_role(BookmarkOperation::FindAll),
                PoolRole::Replica,
                "off mode must never redirect, even after a write"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn effective_role_writes_always_target_primary_regardless_of_pin_state() {
        assert_eq!(
            BookmarkRepository::effective_role(BookmarkOperation::Save),
            PoolRole::Primary,
            "writes are already Primary via role_for, pin or not"
        );
        let pin = RequestPin::new(ReadYourWrites::Request);
        scope(pin, async {
            mark_write();
            assert_eq!(
                BookmarkRepository::effective_role(BookmarkOperation::Save),
                PoolRole::Primary
            );
        })
        .await;
    }

    #[test]
    fn bookmark_shards_wrap_across_fixed_partition_count() {
        assert_eq!(LINK_CHECKER_SHARD_COUNT, 16);
        let shard_count = i64::from(LINK_CHECKER_SHARD_COUNT);

        assert_eq!(0_i64.rem_euclid(shard_count), 0);
        assert_eq!(15_i64.rem_euclid(shard_count), 15);
        assert_eq!(16_i64.rem_euclid(shard_count), 0);
        assert_eq!(31_i64.rem_euclid(shard_count), 15);
    }

    #[test]
    fn advisory_lock_keys_are_stable_per_shard() {
        let shard_0 = BookmarkRepository::link_checker_lock_key(0);
        let shard_15 = BookmarkRepository::link_checker_lock_key(15);

        assert_eq!(shard_0, BookmarkRepository::link_checker_lock_key(0));
        assert_eq!(shard_15, BookmarkRepository::link_checker_lock_key(15));
        assert_ne!(shard_0, shard_15);
    }

    #[tokio::test]
    #[ignore = "requires Docker (testcontainers)"]
    async fn advisory_lock_is_exclusive_and_reacquirable() {
        let db = TestDb::shared().await;
        let config = DistributedConfig::from_urls(db.url(), db.url()).with_pool_sizes(1, 1);
        let pools = create_dual_pools(&config).expect("test pools should build");
        let state = Arc::new(DistributedState::new(config, pools));
        state
            .install_global()
            .expect("distributed state should install");

        let shard = 3;
        let lease = BookmarkRepository::acquire_shard_lease(shard)
            .await
            .expect("first shard lease should be acquired")
            .expect("first shard lease should exist");

        let second_attempt = BookmarkRepository::acquire_shard_lease(shard)
            .await
            .expect("second shard lease attempt should not error");
        assert!(
            second_attempt.is_none(),
            "the shard lock should remain exclusive while held"
        );

        BookmarkRepository::release_shard_lease(lease)
            .await
            .expect("leasing release should succeed");

        let reacquired = BookmarkRepository::acquire_shard_lease(shard)
            .await
            .expect("shard should be reacquirable after release");
        assert!(
            reacquired.is_some(),
            "the shard lock should be available again after release"
        );

        if let Some(lease) = reacquired {
            BookmarkRepository::release_shard_lease(lease)
                .await
                .expect("cleanup release should succeed");
        }
    }

    #[test]
    fn update_missing_row_maps_to_explicit_not_found_error() {
        let error = BookmarkRepository::finish_update_result(Err(DieselError::NotFound), 99)
            .expect_err("missing rows should be reported explicitly");

        assert_eq!(
            error.to_string(),
            "bookmark with id 99 not found during update"
        );
    }

    #[test]
    fn mark_dead_missing_rows_are_tolerated() {
        let updated = BookmarkRepository::finish_mark_dead_result(0, 99)
            .expect("replica lag and concurrent deletes should not abort the task");

        assert!(!updated);
    }

    #[test]
    fn mark_dead_reports_success_when_a_row_was_updated() {
        let updated = BookmarkRepository::finish_mark_dead_result(1, 99)
            .expect("affected rows should be reported as an applied update");

        assert!(updated);
    }

    #[test]
    fn missing_row_errors_are_explicit_for_write_paths() {
        let error = BookmarkRepository::missing_bookmark_error("delete", 99);

        assert_eq!(
            error.to_string(),
            "bookmark with id 99 not found during delete"
        );
    }
}
