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
/// batch/count guards are emitted.
#[autumn_web::repository(NoShardSetPost, table = "no_shard_set_posts", tenant_scoped, sharded)]
pub trait NoShardSetPostRepository {}

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
