//! §1d: `across_tenants()` cross-shard fan-out for
//! `#[repository(tenant_scoped, sharded)]`.
//!
//! Tests that:
//!  - A `#[repository(tenant_scoped, sharded)]` repository exposes an
//!    `across_tenants()` method that returns a clone with `across_tenants: true`.
//!  - `with_pool_untracked(pool).across_tenants()` compiles and chains correctly.
//!  - The generated struct carries the `__autumn_shards` field (Some / None).
//!  - The generated fan-out code path is reachable (no infinite recursion risk)
//!    because `with_pool_untracked` always produces `__autumn_shards: None`.
//!
//! No live database or real `ShardSet` is required — pools are created lazily
//! by deadpool and the tests only inspect struct-level fields.

#![cfg(feature = "db")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db;
use autumn_web::reexports::diesel_async::AsyncPgConnection;
use autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool;

mod schema {
    autumn_web::reexports::diesel::table! {
        sharded_tenant_posts (id) {
            id -> Int8,
            tenant_id -> Text,
            title -> Text,
            deleted_at -> Nullable<Timestamp>,
        }
    }
}

use schema::sharded_tenant_posts;

/// A minimal model living on a tenant-scoped, sharded table.
/// Carries `deleted_at` so the repository can enable `soft_delete` and exercise
/// the cross-shard fan-out / reject codegen for the soft-delete readers.
#[autumn_web::model(table = "sharded_tenant_posts")]
pub struct ShardedTenantPost {
    #[id]
    pub id: i64,
    pub tenant_id: String,
    pub title: String,
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

/// Matching repository — tenant_scoped so `across_tenants()` is generated,
/// sharded so `__autumn_shards` is present and the fan-out guard is emitted,
/// soft_delete so the `with_deleted` / `only_deleted` fan-out and
/// `page_only_deleted` cross-shard reject codegen is compiled.
#[autumn_web::repository(
    ShardedTenantPost,
    table = "sharded_tenant_posts",
    tenant_scoped,
    sharded,
    soft_delete
)]
pub trait ShardedTenantPostRepository {
    // Derived read: must fan out across shards under `across_tenants()` (a
    // generated `__autumn_find_by_title_one_shard` helper). Compiling this
    // exercises the per-shard fan-out codegen for user-declared read methods.
    async fn find_by_title(&self, title: String) -> Vec<ShardedTenantPost>;
    // Derived write: must reject under cross-shard `across_tenants()`.
    async fn delete_by_title(&self, title: String);
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn make_pool() -> Pool<AsyncPgConnection> {
    let config = DatabaseConfig {
        url: Some("postgres://localhost/sharding_test".to_owned()),
        pool_size: 2,
        ..Default::default()
    };
    db::create_pool(&config)
        .expect("pool config must be valid")
        .expect("url must be set")
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[test]
fn sharded_tenant_scoped_repo_exposes_across_tenants_method() {
    // Compiling this test file is the primary assertion: the generated
    // PgShardedTenantPostRepository must have an `across_tenants()` method.
    let pool = make_pool();
    let repo = PgShardedTenantPostRepository::with_pool_untracked(pool);
    // across_tenants() must compile and return Self
    let _across = repo.across_tenants();
}

#[test]
fn with_pool_untracked_has_no_shards() {
    // with_pool_untracked always produces __autumn_shards: None, which is
    // what prevents infinite recursion in the fan-out guard.
    let pool = make_pool();
    let repo = PgShardedTenantPostRepository::with_pool_untracked(pool);
    assert!(
        repo.__autumn_shards.is_none(),
        "with_pool_untracked must yield __autumn_shards = None to prevent fan-out recursion"
    );
}

#[test]
fn across_tenants_on_pool_untracked_repo_has_no_shards() {
    // with_pool_untracked(pool).across_tenants() must still have
    // __autumn_shards = None — the fan-out guard only fires when Some.
    let pool = make_pool();
    let repo = PgShardedTenantPostRepository::with_pool_untracked(pool).across_tenants();
    assert!(
        repo.__autumn_shards.is_none(),
        "across_tenants() on a with_pool_untracked repo must keep __autumn_shards = None"
    );
}
