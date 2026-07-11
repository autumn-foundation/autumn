//! §1692: cross-shard `across_tenants()` guards must fire in the no-shard-set
//! case for a `#[repository(tenant_scoped, sharded)]`.
//!
//! A repo built via `with_pool_untracked(pool)` carries `__autumn_shards: None`.
//! Under `across_tenants()`:
//!  - `find_in_batches` / `find_each` must REJECT (batched iteration cannot fan
//!    out across shards), rather than silently streaming a PARTIAL result over
//!    only the current pool.
//!  - `count` (legitimately mergeable across shards) must REJECT when no shard
//!    set is configured, rather than falling through to a partial single-pool
//!    count.
//!
//! §1741: the identical no-shard-set hole existed in the sibling read methods —
//! `find_all`, `find_by_id`, `exists_by_id`, derived `find_by_*`, and the
//! soft-delete readers `with_deleted` / `only_deleted`. Each fanned out only
//! when a shard set was present and otherwise fell through to a single-pool
//! query binding a NULL tenant predicate (a silent PARTIAL result). They now
//! reject under `across_tenants()` with no shard set, matching the count guard.
//!
//! Both guards return before acquiring any database connection, so — like the
//! read-routing test in `repository_find_in_batches.rs` — no live database is
//! needed and these assertions run without Docker.

#![cfg(feature = "db")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db;
use autumn_web::reexports::diesel_async::AsyncPgConnection;
use autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool;

mod schema {
    autumn_web::reexports::diesel::table! {
        no_shard_set_posts (id) {
            id -> Int8,
            tenant_id -> Text,
            title -> Text,
        }
    }
}

use schema::no_shard_set_posts;

/// A minimal model living on a tenant-scoped, sharded table.
#[autumn_web::model(table = "no_shard_set_posts")]
pub struct NoShardSetPost {
    #[id]
    pub id: i64,
    pub tenant_id: String,
    pub title: String,
}

/// tenant_scoped so `across_tenants()` is generated; sharded so the cross-shard
/// batch/count guards are emitted. A derived `find_by_title` read exercises the
/// derived `find_by_*` fan-out guard (#1741).
#[autumn_web::repository(NoShardSetPost, table = "no_shard_set_posts", tenant_scoped, sharded)]
pub trait NoShardSetPostRepository {
    async fn find_by_title(&self, title: String) -> autumn_web::AutumnResult<Vec<NoShardSetPost>>;
}

mod soft_schema {
    autumn_web::reexports::diesel::table! {
        soft_no_shard_set_posts (id) {
            id -> Int8,
            tenant_id -> Text,
            title -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use soft_schema::soft_no_shard_set_posts;

/// A model carrying `deleted_at` on a tenant-scoped, sharded table, so its
/// repository can enable `soft_delete` and the `with_deleted` / `only_deleted`
/// cross-shard readers are emitted (#1741).
#[autumn_web::model(table = "soft_no_shard_set_posts")]
pub struct SoftNoShardSetPost {
    #[id]
    pub id: i64,
    pub tenant_id: String,
    pub title: String,
    pub deleted_at: Option<autumn_web::reexports::chrono::NaiveDateTime>,
}

#[autumn_web::repository(
    SoftNoShardSetPost,
    table = "soft_no_shard_set_posts",
    tenant_scoped,
    sharded,
    soft_delete
)]
pub trait SoftNoShardSetPostRepository {}

fn make_pool() -> Pool<AsyncPgConnection> {
    let config = DatabaseConfig {
        url: Some("postgres://localhost/no_shard_set_test".to_owned()),
        pool_size: 2,
        ..Default::default()
    };
    db::create_pool(&config)
        .expect("pool config must be valid")
        .expect("url must be set")
}

/// `across_tenants()` on a `with_pool_untracked` repo has `__autumn_shards =
/// None`; batched iteration must reject rather than silently returning a partial
/// single-pool result. The guard fires before any connection is acquired, so no
/// live database is required.
#[tokio::test]
async fn find_in_batches_across_tenants_without_shard_set_rejects() {
    let pool = make_pool();
    let repo = PgNoShardSetPostRepository::with_pool_untracked(pool).across_tenants();
    assert!(
        repo.__autumn_shards.is_none(),
        "with_pool_untracked must yield __autumn_shards = None"
    );

    let mut batches = repo.find_in_batches(50);
    let err = batches
        .next_batch()
        .await
        .expect_err("across_tenants batched iteration without a shard set must reject");
    assert!(
        err.to_string()
            .contains("cross-shard batched iteration is not supported"),
        "batch guard must reject cross-shard iteration, got: {err}"
    );

    // `find_each` shares the same BatchSource and must reject identically.
    let mut each = repo.find_each(50);
    let err = each
        .next()
        .await
        .expect_err("across_tenants find_each without a shard set must reject");
    assert!(
        err.to_string()
            .contains("cross-shard batched iteration is not supported"),
        "find_each must reject cross-shard iteration, got: {err}"
    );
}

/// `across_tenants().count()` on a `with_pool_untracked` repo (no shard set)
/// must reject rather than falling through to a partial single-pool count. The
/// guard fires before any connection is acquired, so no live database is
/// required.
#[tokio::test]
async fn count_across_tenants_without_shard_set_rejects() {
    let pool = make_pool();
    let repo = PgNoShardSetPostRepository::with_pool_untracked(pool).across_tenants();
    assert!(repo.__autumn_shards.is_none());

    let err = repo
        .count()
        .await
        .expect_err("across_tenants count without a shard set must reject");
    assert!(
        err.to_string()
            .contains("cross-shard count requires a configured shard set"),
        "count guard must reject cross-shard count without a shard set, got: {err}"
    );
}

/// §1741: `find_all().across_tenants()` on a no-shard-set repo must reject
/// rather than silently returning a single-pool partial result.
#[tokio::test]
async fn find_all_across_tenants_without_shard_set_rejects() {
    let pool = make_pool();
    let repo = PgNoShardSetPostRepository::with_pool_untracked(pool).across_tenants();
    assert!(repo.__autumn_shards.is_none());

    let err = repo
        .find_all()
        .await
        .expect_err("across_tenants find_all without a shard set must reject");
    assert!(
        err.to_string()
            .contains("cross-shard find_all requires a configured shard set"),
        "find_all guard must reject cross-shard read without a shard set, got: {err}"
    );
}

/// §1741: `find_by_id().across_tenants()` on a no-shard-set repo must reject.
#[tokio::test]
async fn find_by_id_across_tenants_without_shard_set_rejects() {
    let pool = make_pool();
    let repo = PgNoShardSetPostRepository::with_pool_untracked(pool).across_tenants();
    assert!(repo.__autumn_shards.is_none());

    let err = repo
        .find_by_id(1)
        .await
        .expect_err("across_tenants find_by_id without a shard set must reject");
    assert!(
        err.to_string()
            .contains("cross-shard find_by_id requires a configured shard set"),
        "find_by_id guard must reject cross-shard read without a shard set, got: {err}"
    );
}

/// §1741: `exists_by_id().across_tenants()` on a no-shard-set repo must reject.
#[tokio::test]
async fn exists_by_id_across_tenants_without_shard_set_rejects() {
    let pool = make_pool();
    let repo = PgNoShardSetPostRepository::with_pool_untracked(pool).across_tenants();
    assert!(repo.__autumn_shards.is_none());

    let err = repo
        .exists_by_id(1)
        .await
        .expect_err("across_tenants exists_by_id without a shard set must reject");
    assert!(
        err.to_string()
            .contains("cross-shard exists_by_id requires a configured shard set"),
        "exists_by_id guard must reject cross-shard read without a shard set, got: {err}"
    );
}

/// §1741: a derived `find_by_*` read with `across_tenants()` on a no-shard-set
/// repo must reject rather than fanning out or returning a partial result.
#[tokio::test]
async fn find_by_derived_across_tenants_without_shard_set_rejects() {
    let pool = make_pool();
    let repo = PgNoShardSetPostRepository::with_pool_untracked(pool).across_tenants();
    assert!(repo.__autumn_shards.is_none());

    let err = repo
        .find_by_title("hello".to_owned())
        .await
        .expect_err("across_tenants find_by_title without a shard set must reject");
    assert!(
        err.to_string()
            .contains("cross-shard find_by_title requires a configured shard set"),
        "derived find_by_* guard must reject cross-shard read without a shard set, got: {err}"
    );
}

/// §1741: `with_deleted().across_tenants()` on a no-shard-set soft-delete repo
/// must reject.
#[tokio::test]
async fn with_deleted_across_tenants_without_shard_set_rejects() {
    let pool = make_pool();
    let repo = PgSoftNoShardSetPostRepository::with_pool_untracked(pool).across_tenants();
    assert!(repo.__autumn_shards.is_none());

    let err = repo
        .with_deleted()
        .await
        .expect_err("across_tenants with_deleted without a shard set must reject");
    assert!(
        err.to_string()
            .contains("cross-shard with_deleted requires a configured shard set"),
        "with_deleted guard must reject cross-shard read without a shard set, got: {err}"
    );
}

/// §1741: `only_deleted().across_tenants()` on a no-shard-set soft-delete repo
/// must reject.
#[tokio::test]
async fn only_deleted_across_tenants_without_shard_set_rejects() {
    let pool = make_pool();
    let repo = PgSoftNoShardSetPostRepository::with_pool_untracked(pool).across_tenants();
    assert!(repo.__autumn_shards.is_none());

    let err = repo
        .only_deleted()
        .await
        .expect_err("across_tenants only_deleted without a shard set must reject");
    assert!(
        err.to_string()
            .contains("cross-shard only_deleted requires a configured shard set"),
        "only_deleted guard must reject cross-shard read without a shard set, got: {err}"
    );
}
