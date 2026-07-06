//! Database-level integration tests for bulk repository CRUD operations (issue #841).
//!
//! **Requires Docker** to be running.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::hooks::{MutationContext, MutationHooks, Patch, UpdateDraft};
use autumn_web::prelude::*;
use diesel::prelude::*;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Schema & models without hooks ────────────────────────────────────────────

diesel::table! {
    test_bulk_records (id) {
        id -> Int8,
        name -> Text,
        value -> Int4,
    }
}

#[autumn_web::model(table = "test_bulk_records")]
#[derive(PartialEq, Eq)]
pub struct BulkRecord {
    #[id]
    pub id: i64,
    pub name: String,
    pub value: i32,
}

#[autumn_web::repository(BulkRecord, table = "test_bulk_records")]
pub trait BulkRecordRepository {}

// ── Schema & models for LockRecord (optimistic locking) ──────────────────────

diesel::table! {
    test_lock_records (id) {
        id -> Int8,
        name -> Text,
        lock_version -> Int8,
    }
}

#[autumn_web::model(table = "test_lock_records")]
pub struct LockRecord {
    #[id]
    pub id: i64,
    pub name: String,
    #[lock_version]
    pub lock_version: i64,
}

#[autumn_web::repository(LockRecord, table = "test_lock_records")]
pub trait LockRecordRepository {}

// ── Schema & models for zero-column writable safety (Task 2) ──────────────────

diesel::table! {
    test_zero_col_records (id) {
        id -> Int8,
        dummy -> Text,
    }
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    diesel::Queryable,
    diesel::Selectable,
    diesel::Insertable,
    diesel::AsChangeset,
    serde::Serialize,
    serde::Deserialize,
)]
#[diesel(table_name = test_zero_col_records)]
pub struct ZeroColRecord {
    pub id: i64,
    pub dummy: String,
}

#[derive(Debug, Clone, diesel::Insertable, serde::Serialize, serde::Deserialize)]
#[diesel(table_name = test_zero_col_records)]
pub struct NewZeroColRecord {
    pub dummy: String,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[allow(dead_code)]
pub struct UpdateZeroColRecord {
    #[serde(default)]
    pub dummy: ::autumn_web::hooks::Patch<String>,
}

#[doc(hidden)]
#[derive(Debug, Clone, diesel::AsChangeset)]
#[diesel(table_name = test_zero_col_records)]
pub struct __ZeroColRecordChangeset {
    pub dummy: Option<String>,
}

impl UpdateZeroColRecord {
    pub fn __to_changeset(&self) -> __ZeroColRecordChangeset {
        __ZeroColRecordChangeset {
            dummy: match &self.dummy {
                ::autumn_web::hooks::Patch::Set(v) => Some(v.clone()),
                _ => None,
            },
        }
    }
}

impl ::autumn_web::repository::AutumnColumnCountExt for ZeroColRecord {
    fn __autumn_column_count(&self) -> usize {
        0
    }
}

impl ::autumn_web::repository::AutumnColumnCountExt for NewZeroColRecord {
    fn __autumn_column_count(&self) -> usize {
        0
    }
}

impl ::autumn_web::tenancy::ModelTenantIdMeta for ZeroColRecord {
    const HAS_MANUAL_TENANT_ID: bool = false;
    fn try_set_tenant_id(&mut self, _tenant_id: &str) {}
}

impl ::autumn_web::tenancy::ModelTenantIdMeta for NewZeroColRecord {
    const HAS_MANUAL_TENANT_ID: bool = false;
    fn try_set_tenant_id(&mut self, _tenant_id: &str) {}
}

impl ZeroColRecord {
    #[allow(clippy::unused_self)]
    pub fn __autumn_lock_version_actual(&self) -> ::core::option::Option<i64> {
        None
    }

    pub fn __autumn_upsert_set() -> impl ::autumn_web::reexports::diesel::query_builder::AsChangeset<
        Target = self::test_zero_col_records::table,
        Changeset = impl ::autumn_web::reexports::diesel::query_builder::QueryFragment<
            ::autumn_web::reexports::diesel::pg::Pg,
        > + ::core::marker::Send
                    + ::core::marker::Sync
                    + 'static,
    > + ::core::marker::Send
    + ::core::marker::Sync
    + 'static {
        use self::test_zero_col_records::dsl::*;
        use ::autumn_web::reexports::diesel::ExpressionMethods as _;
        (dummy.eq(::autumn_web::reexports::diesel::pg::upsert::excluded(dummy)),)
    }
}

#[autumn_web::repository(ZeroColRecord, table = "test_zero_col_records", no_upsert_trait)]
pub trait ZeroColRecordRepository {}

// ── Schema & models for Tenant scoped bulk operations (Task 3 & 4) ────────────

diesel::table! {
    test_tenant_bulk_records (id) {
        id -> Int8,
        name -> Text,
        tenant_id -> Text,
    }
}

#[autumn_web::model(table = "test_tenant_bulk_records")]
pub struct TenantBulkRecord {
    #[id]
    pub id: i64,
    pub name: String,
    #[default]
    pub tenant_id: String,
}

#[autumn_web::repository(TenantBulkRecord, table = "test_tenant_bulk_records", tenant_scoped)]
pub trait TenantBulkRecordRepository {}

// ── Schema & models with hooks ───────────────────────────────────────────────

diesel::table! {
    test_hooked_records (id) {
        id -> Int8,
        name -> Text,
        value -> Int4,
    }
}

#[autumn_web::model(table = "test_hooked_records")]
#[derive(PartialEq, Eq)]
pub struct HookedRecord {
    #[id]
    pub id: i64,
    pub name: String,
    pub value: i32,
}

#[derive(Clone, Default)]
pub struct HookedRecordHooks;

impl MutationHooks for HookedRecordHooks {
    type Model = HookedRecord;
    type NewModel = NewHookedRecord;
    type UpdateModel = UpdateHookedRecord;

    async fn before_create(
        &self,
        _ctx: &mut MutationContext,
        new: &mut NewHookedRecord,
    ) -> AutumnResult<()> {
        if new.name == "invalid" {
            return Err(autumn_web::AutumnError::bad_request_msg("invalid name"));
        }
        if new.name == "hook_modified" {
            new.value = 999;
        }
        Ok(())
    }

    async fn before_update(
        &self,
        _ctx: &mut MutationContext,
        draft: &mut UpdateDraft<HookedRecord>,
    ) -> AutumnResult<()> {
        if draft.after.name == "invalid_update" {
            return Err(autumn_web::AutumnError::bad_request_msg(
                "invalid name on update",
            ));
        }
        if draft.after.name == "hook_modified_update" {
            draft.after.value = 777;
        }
        Ok(())
    }
}

#[autumn_web::repository(HookedRecord, table = "test_hooked_records", hooks = HookedRecordHooks)]
pub trait HookedRecordRepository {}

// ── Schema & models with hooks and version history (Task 20) ──────────────────

diesel::table! {
    test_hooked_versioned_records (id) {
        id -> Int8,
        name -> Text,
        value -> Int4,
    }
}

#[autumn_web::model(table = "test_hooked_versioned_records")]
#[derive(PartialEq, Eq)]
pub struct HookedVersionedRecord {
    #[id]
    pub id: i64,
    pub name: String,
    pub value: i32,
}

#[derive(Clone, Default)]
pub struct HookedVersionedRecordHooks;

impl MutationHooks for HookedVersionedRecordHooks {
    type Model = HookedVersionedRecord;
    type NewModel = NewHookedVersionedRecord;
    type UpdateModel = UpdateHookedVersionedRecord;
}

#[autumn_web::repository(HookedVersionedRecord, table = "test_hooked_versioned_records", hooks = HookedVersionedRecordHooks, versioned = true)]
pub trait HookedVersionedRecordRepository {}

// ── Setup & helpers ─────────────────────────────────────────────────────────

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
    let pool = Pool::builder(manager).max_size(5).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query("CREATE TABLE IF NOT EXISTS test_bulk_records (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, value INT NOT NULL)")
        .execute(&mut conn)
        .await
        .expect("create test_bulk_records");
    diesel::sql_query("CREATE TABLE IF NOT EXISTS test_hooked_records (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, value INT NOT NULL)")
        .execute(&mut conn)
        .await
        .expect("create test_hooked_records");
    diesel::sql_query("CREATE TABLE IF NOT EXISTS test_lock_records (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, lock_version INT8 NOT NULL DEFAULT 1)")
        .execute(&mut conn)
        .await
        .expect("create test_lock_records");
    diesel::sql_query("CREATE TABLE IF NOT EXISTS test_zero_col_records (id BIGSERIAL PRIMARY KEY, dummy TEXT NOT NULL)")
        .execute(&mut conn)
        .await
        .expect("create test_zero_col_records");
    diesel::sql_query("CREATE TABLE IF NOT EXISTS test_tenant_bulk_records (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, tenant_id TEXT NOT NULL)")
        .execute(&mut conn)
        .await
        .expect("create test_tenant_bulk_records");
    diesel::sql_query("CREATE TABLE IF NOT EXISTS test_hooked_versioned_records (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, value INT NOT NULL)")
        .execute(&mut conn)
        .await
        .expect("create test_hooked_versioned_records");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS _autumn_version_history (
        id          BIGSERIAL   PRIMARY KEY,
        table_name  TEXT        NOT NULL,
        tenant_id   TEXT,
        record_id   BIGINT      NOT NULL,
        op          TEXT        NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
        actor       TEXT        NOT NULL DEFAULT 'system',
        request_id  TEXT,
        changes     JSONB       NOT NULL DEFAULT '[]',
        recorded_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
    )",
    )
    .execute(&mut conn)
    .await
    .expect("create _autumn_version_history");

    (pool, container)
}

const fn build_lock_repo(pool: Pool<AsyncPgConnection>) -> PgLockRecordRepository {
    PgLockRecordRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

const fn build_bulk_repo(pool: Pool<AsyncPgConnection>) -> PgBulkRecordRepository {
    PgBulkRecordRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

const fn build_hooked_repo(pool: Pool<AsyncPgConnection>) -> PgHookedRecordRepository {
    PgHookedRecordRepository {
        pool,
        hooks: HookedRecordHooks,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

const fn build_zero_col_repo(pool: Pool<AsyncPgConnection>) -> PgZeroColRecordRepository {
    PgZeroColRecordRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

const fn build_tenant_bulk_repo(pool: Pool<AsyncPgConnection>) -> PgTenantBulkRecordRepository {
    PgTenantBulkRecordRepository {
        pool,
        across_tenants: false,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

const fn build_hooked_versioned_repo(
    pool: Pool<AsyncPgConnection>,
) -> PgHookedVersionedRecordRepository {
    PgHookedVersionedRecordRepository {
        pool,
        hooks: HookedVersionedRecordHooks,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

// ── Tests (RED - expects compile errors until green phase is implemented) ────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_bulk_ops_without_hooks() {
    let (pool, _container) = setup_pool().await;
    let repo = build_bulk_repo(pool);

    // 1. save_many
    let new_records = vec![
        NewBulkRecord {
            name: "A".to_string(),
            value: 10,
        },
        NewBulkRecord {
            name: "B".to_string(),
            value: 20,
        },
        NewBulkRecord {
            name: "C".to_string(),
            value: 30,
        },
    ];
    let inserted = repo.save_many(&new_records).await.unwrap();
    assert_eq!(inserted.len(), 3);
    assert_eq!(inserted[0].name, "A");
    assert_eq!(inserted[1].name, "B");
    assert_eq!(inserted[2].name, "C");
    assert!(inserted[0].id > 0);

    // 2. update_many
    let ids = vec![inserted[0].id, inserted[1].id];
    let changes = UpdateBulkRecord {
        name: Patch::Set("Updated".to_string()),
        value: Patch::Set(100),
    };
    let updated = repo.update_many(&ids, &changes).await.unwrap();
    assert_eq!(updated.len(), 2);
    for row in &updated {
        assert_eq!(row.name, "Updated");
        assert_eq!(row.value, 100);
    }

    // 3. upsert_many
    let mut upsert_records = inserted.clone();
    upsert_records[0].name = "Upserted A".to_string(); // Existing row update
    upsert_records[0].value = 500;

    // Create a new record representation with a custom/new ID for insertion
    let mut conn = repo.pool.get().await.unwrap();
    let max_id: i64 = test_bulk_records::table
        .select(diesel::dsl::max(test_bulk_records::id))
        .first::<Option<i64>>(&mut conn)
        .await
        .unwrap()
        .unwrap_or(0);
    let new_row_id = max_id + 10;
    upsert_records.push(BulkRecord {
        id: new_row_id,
        name: "Upserted New".to_string(),
        value: 1000,
    });

    let upserted = repo.upsert_many(&upsert_records).await.unwrap();
    assert_eq!(upserted.len(), 4);

    let db_rows = repo.find_all().await.unwrap();
    assert_eq!(db_rows.len(), 4);
    let row_a = db_rows.iter().find(|r| r.id == inserted[0].id).unwrap();
    assert_eq!(row_a.name, "Upserted A");
    assert_eq!(row_a.value, 500);

    let row_new = db_rows.iter().find(|r| r.id == new_row_id).unwrap();
    assert_eq!(row_new.name, "Upserted New");
    assert_eq!(row_new.value, 1000);

    // 4. delete_many
    let all_ids: Vec<i64> = db_rows.iter().map(|r| r.id).collect();
    repo.delete_many(&all_ids).await.unwrap();
    let after_delete = repo.find_all().await.unwrap();
    assert!(after_delete.is_empty());
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_bulk_ops_with_hooks() {
    let (pool, _container) = setup_pool().await;
    let repo = build_hooked_repo(pool);

    // 1. save_many (happy path and modified hook path)
    let new_records = vec![
        NewHookedRecord {
            name: "Normal".to_string(),
            value: 10,
        },
        NewHookedRecord {
            name: "hook_modified".to_string(),
            value: 20,
        },
    ];
    let inserted = repo.save_many(&new_records).await.unwrap();
    assert_eq!(inserted.len(), 2);
    assert_eq!(inserted[0].name, "Normal");
    assert_eq!(inserted[0].value, 10);
    assert_eq!(inserted[1].name, "hook_modified");
    assert_eq!(inserted[1].value, 999); // value overwritten by before_create hook

    // 2. save_many (failure path - aborts whole transaction)
    let bad_records = vec![
        NewHookedRecord {
            name: "Valid".to_string(),
            value: 30,
        },
        NewHookedRecord {
            name: "invalid".to_string(),
            value: 40,
        }, // triggers Err in before_create hook
    ];
    let err = repo.save_many(&bad_records).await.unwrap_err();
    assert!(err.to_string().contains("invalid name"));

    // Verify transaction rolled back (no "Valid" record with value 30 exists)
    let db_rows = repo.find_all().await.unwrap();
    assert_eq!(db_rows.len(), 2);
    assert!(db_rows.iter().all(|r| r.value != 30));

    // 3. save_many_skip_invalid (happy path with invalid filtering and DB constraint fallback)
    let mix_records = vec![
        NewHookedRecord {
            name: "Valid Mix 1".to_string(),
            value: 100,
        },
        NewHookedRecord {
            name: "invalid".to_string(),
            value: 200,
        }, // failed hook
        NewHookedRecord {
            name: "Valid Mix 2".to_string(),
            value: 300,
        },
    ];
    let (successes, failures) = repo.save_many_skip_invalid(&mix_records).await.unwrap();
    assert_eq!(successes.len(), 2);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, 1); // index 1 failed
    assert!(failures[0].1.to_string().contains("invalid name"));

    // 4. update_many (happy path and modified hook path)
    let ids = vec![inserted[0].id, inserted[1].id];
    let changes = UpdateHookedRecord {
        name: Patch::Set("hook_modified_update".to_string()),
        value: Patch::Set(50),
    };
    let updated = repo.update_many(&ids, &changes).await.unwrap();
    assert_eq!(updated.len(), 2);
    for row in &updated {
        assert_eq!(row.name, "hook_modified_update");
        assert_eq!(row.value, 777); // value overwritten by before_update hook
    }

    // 5. update_many (failure path - aborts whole transaction)
    let bad_changes = UpdateHookedRecord {
        name: Patch::Set("invalid_update".to_string()),
        value: Patch::Set(50),
    };
    let err_update = repo.update_many(&ids, &bad_changes).await.unwrap_err();
    assert!(err_update.to_string().contains("invalid name on update"));

    // Verify transaction rolled back (still "hook_modified_update" with value 777)
    let db_rows_after = repo.find_all().await.unwrap();
    assert!(
        db_rows_after
            .iter()
            .all(|r| r.value == 777 || r.value == 100 || r.value == 300)
    );

    // 6. delete_many
    let all_ids: Vec<i64> = db_rows_after.iter().map(|r| r.id).collect();
    repo.delete_many(&all_ids).await.unwrap();
    let after_delete = repo.find_all().await.unwrap();
    assert!(after_delete.is_empty());
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_bulk_ops_optimistic_locking() {
    let (pool, _container) = setup_pool().await;
    let repo = build_lock_repo(pool);

    // 1. Save initial lock records (lock_version defaults to 1 in DB)
    let new_records = vec![
        NewLockRecord {
            name: "Lock A".to_string(),
        },
        NewLockRecord {
            name: "Lock B".to_string(),
        },
    ];
    let inserted = repo.save_many(&new_records).await.unwrap();
    assert_eq!(inserted.len(), 2);
    assert_eq!(inserted[0].lock_version, 1);
    assert_eq!(inserted[1].lock_version, 1);

    // 2. Successful bulk update: match expected lock version (1)
    let ids = vec![inserted[0].id, inserted[1].id];
    let changes = UpdateLockRecord {
        name: Patch::Set("Lock Updated".to_string()),
        lock_version: 1, // Matches!
    };
    let updated = repo.update_many(&ids, &changes).await.unwrap();
    assert_eq!(updated.len(), 2);
    assert_eq!(updated[0].name, "Lock Updated");
    assert_eq!(updated[0].lock_version, 2); // Auto-incremented in DB!
    assert_eq!(updated[1].name, "Lock Updated");
    assert_eq!(updated[1].lock_version, 2);

    // 3. Failed bulk update: mismatch in expected lock version
    let bad_changes = UpdateLockRecord {
        name: Patch::Set("Lock Failed Update".to_string()),
        lock_version: 1, // Expected 1, but actual is 2!
    };
    let err = repo.update_many(&ids, &bad_changes).await.unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("conflict") || err_str.contains("Conflict"),
        "Expected conflict error, got: {err_str}"
    );

    // Verify database rows did not change and transaction rolled back
    let final_rows = repo.find_all().await.unwrap();
    assert_eq!(final_rows.len(), 2);
    for row in &final_rows {
        assert_eq!(row.name, "Lock Updated");
        assert_eq!(row.lock_version, 2);
    }
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_zero_col_bulk_insert() {
    let (pool, _container) = setup_pool().await;
    let repo = build_zero_col_repo(pool);

    let new_records = vec![
        NewZeroColRecord {
            dummy: "A".to_string(),
        },
        NewZeroColRecord {
            dummy: "B".to_string(),
        },
    ];
    let inserted = repo.save_many(&new_records).await.unwrap();
    assert_eq!(inserted.len(), 2);
    assert!(inserted[0].id > 0);
    assert!(inserted[1].id > inserted[0].id);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_tenant_scoped_upsert_many_mismatch() {
    use autumn_web::tenancy::with_tenant;

    let (pool, _container) = setup_pool().await;
    let repo = build_tenant_bulk_repo(pool);

    // 1. Create a record for tenant-a
    let record_a = with_tenant("tenant-a".to_string(), async {
        repo.save(&NewTenantBulkRecord {
            name: "A".to_string(),
        })
        .await
        .unwrap()
    })
    .await;

    // 2. Try to upsert the same ID under tenant-b context
    let stale_or_mismatched = with_tenant("tenant-b".to_string(), async {
        let mut to_upsert = record_a.clone();
        to_upsert.name = "B".to_string(); // Try to overwrite it or update it

        repo.upsert_many(&[to_upsert]).await
    })
    .await;

    // This must return a conflict/bad_request error due to tenant mismatch
    assert!(
        stale_or_mismatched.is_err(),
        "Expected tenant mismatch error, but it succeeded!"
    );
    let err_str = stale_or_mismatched.unwrap_err().to_string();
    assert!(
        err_str.contains("Tenant conflict") || err_str.contains("potential cross-tenant conflict"),
        "Expected cross-tenant conflict error, got: {err_str}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_lock_version_upsert_many_conflict() {
    let (pool, _container) = setup_pool().await;
    let repo = build_lock_repo(pool);

    // 1. Save initial lock records
    let inserted = repo
        .save_many(&[NewLockRecord {
            name: "Lock A".to_string(),
        }])
        .await
        .unwrap();
    assert_eq!(inserted.len(), 1);
    let original = inserted[0].clone();
    assert_eq!(original.lock_version, 1);

    // 2. Perform a successful upsert with correct lock version
    let mut to_upsert = original.clone();
    to_upsert.name = "Lock A (Updated)".to_string();
    let upserted = repo.upsert_many(&[to_upsert.clone()]).await.unwrap();
    assert_eq!(upserted.len(), 1);
    assert_eq!(upserted[0].name, "Lock A (Updated)");
    assert_eq!(upserted[0].lock_version, 2); // DB increments it!

    // 3. Stale upsert with outdated lock version should fail with Conflict
    let mut stale_upsert = original.clone(); // Has lock_version = 1, but actual in DB is 2
    stale_upsert.name = "Lock A (Stale)".to_string();
    let stale_res = repo.upsert_many(&[stale_upsert]).await;

    assert!(
        stale_res.is_err(),
        "Expected stale lock version conflict, but it succeeded!"
    );
    let err_str = stale_res.unwrap_err().to_string();
    assert!(
        err_str.contains("conflict") || err_str.contains("Conflict"),
        "Expected conflict error, got: {err_str}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn test_hooked_versioned_delete_many_records_history() {
    let (pool, _container) = setup_pool().await;
    let repo = build_hooked_versioned_repo(pool);

    // Save initial records
    let new_records = vec![
        NewHookedVersionedRecord {
            name: "H1".to_string(),
            value: 10,
        },
        NewHookedVersionedRecord {
            name: "H2".to_string(),
            value: 20,
        },
    ];
    let inserted = repo.save_many(&new_records).await.unwrap();
    assert_eq!(inserted.len(), 2);
    let ids: Vec<i64> = inserted.iter().map(|r| r.id).collect();

    // Delete them
    repo.delete_many(&ids).await.unwrap();

    // Check version history for both records
    for id in ids {
        let history = repo
            .version_history(id, ::autumn_web::version_history::VersionFilter::default())
            .await
            .unwrap();
        assert!(
            history
                .entries
                .iter()
                .any(|entry| entry.op == ::autumn_web::version_history::VersionOp::Delete),
            "expected a delete entry in version history for record {id}"
        );
    }
}
