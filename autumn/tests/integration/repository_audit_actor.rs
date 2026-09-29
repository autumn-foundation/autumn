//! Database-level integration tests for auto-attributing version-history writes
//! to the ambient current actor (issue #1383).
//!
//! **Requires Docker** to be running.
//!
//! These exercise the `#[repository(versioned)]` write path end-to-end:
//! - inside `with_actor("alice", …)` the resulting `VersionEntry.actor` is the
//!   scoped actor (covers both the non-hooked `Current::actor()` read and the
//!   hooked `MutationContext.actor` seed);
//! - with no actor scope in effect the entry falls back to `SYSTEM_ACTOR`.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::current::with_actor;
use autumn_web::hooks::{MutationHooks, Patch};
use autumn_web::version_history::{VersionFilter, VersionOp};
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Non-hooked versioned repo (exercises the direct `Current::actor()` read) ──

diesel::table! {
    test_audit_actor_records (id) {
        id -> Int8,
        name -> Text,
    }
}

#[autumn_web::model(table = "test_audit_actor_records")]
#[derive(PartialEq, Eq)]
pub struct AuditActorRecord {
    #[id]
    pub id: i64,
    pub name: String,
}

#[autumn_web::repository(AuditActorRecord, table = "test_audit_actor_records", versioned = true)]
pub trait AuditActorRecordRepository {}

// ── Hooked versioned repo (exercises the `MutationContext.actor` seed) ────────

diesel::table! {
    test_audit_actor_hooked_records (id) {
        id -> Int8,
        name -> Text,
    }
}

#[autumn_web::model(table = "test_audit_actor_hooked_records")]
#[derive(PartialEq, Eq)]
pub struct AuditActorHookedRecord {
    #[id]
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Default)]
pub struct AuditActorHookedRecordHooks;

impl MutationHooks for AuditActorHookedRecordHooks {
    type Model = AuditActorHookedRecord;
    type NewModel = NewAuditActorHookedRecord;
    type UpdateModel = UpdateAuditActorHookedRecord;
}

#[autumn_web::repository(
    AuditActorHookedRecord,
    table = "test_audit_actor_hooked_records",
    hooks = AuditActorHookedRecordHooks,
    versioned = true
)]
pub trait AuditActorHookedRecordRepository {}

// ── Setup & helpers ──────────────────────────────────────────────────────────

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
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_audit_actor_records \
         (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_audit_actor_records");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_audit_actor_hooked_records \
         (id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_audit_actor_hooked_records");
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

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgAuditActorRecordRepository {
    PgAuditActorRecordRepository {
        pool,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

const fn build_hooked_repo(pool: Pool<AsyncPgConnection>) -> PgAuditActorHookedRecordRepository {
    PgAuditActorHookedRecordRepository {
        pool,
        hooks: AuditActorHookedRecordHooks,
        __autumn_read_route: autumn_web::repository::ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

/// Actor recorded on the version entry for `op` on record `id`.
async fn actor_for(repo: &PgAuditActorRecordRepository, id: i64, op: VersionOp) -> Option<String> {
    let history = repo
        .version_history(id, VersionFilter::default())
        .await
        .unwrap();
    history
        .entries
        .into_iter()
        .find(|entry| entry.op == op)
        .map(|entry| entry.actor)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn non_hooked_versioned_writes_record_scoped_actor() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    // create / update / delete all inside an explicit actor scope.
    let id = with_actor("alice", async {
        let created = repo
            .save(&NewAuditActorRecord {
                name: "row".to_string(),
            })
            .await
            .unwrap();
        repo.update(
            created.id,
            &UpdateAuditActorRecord {
                name: Patch::Set("row-2".to_string()),
            },
        )
        .await
        .unwrap();
        repo.delete_by_id(created.id).await.unwrap();
        created.id
    })
    .await;

    assert_eq!(
        actor_for(&repo, id, VersionOp::Insert).await.as_deref(),
        Some("alice"),
        "insert history must record the scoped actor"
    );
    assert_eq!(
        actor_for(&repo, id, VersionOp::Update).await.as_deref(),
        Some("alice"),
        "update history must record the scoped actor"
    );
    assert_eq!(
        actor_for(&repo, id, VersionOp::Delete).await.as_deref(),
        Some("alice"),
        "delete history must record the scoped actor"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn non_hooked_versioned_write_without_scope_falls_back_to_system() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    // No actor scope in effect: falls back to SYSTEM_ACTOR.
    let created = repo
        .save(&NewAuditActorRecord {
            name: "unscoped".to_string(),
        })
        .await
        .unwrap();

    assert_eq!(
        actor_for(&repo, created.id, VersionOp::Insert)
            .await
            .as_deref(),
        Some("system"),
        "an unscoped write must fall back to the system actor"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn hooked_versioned_write_records_scoped_actor_via_mutation_context() {
    let (pool, _container) = setup_pool().await;
    let repo = build_hooked_repo(pool);

    let id = with_actor("bob", async {
        let created = repo
            .save(&NewAuditActorHookedRecord {
                name: "hooked".to_string(),
            })
            .await
            .unwrap();
        created.id
    })
    .await;

    let history = repo
        .version_history(id, VersionFilter::default())
        .await
        .unwrap();
    let insert = history
        .entries
        .iter()
        .find(|entry| entry.op == VersionOp::Insert)
        .expect("expected an insert history entry");
    assert_eq!(
        insert.actor, "bob",
        "hooked versioned insert must seed MutationContext.actor from the scoped actor"
    );
}
