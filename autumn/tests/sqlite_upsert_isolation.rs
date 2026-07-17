//! `#[repository]` upsert isolation + optimistic-lock proof on the `SQLite`
//! runtime backend (issue #1996, PR #2021).
//!
//! Pins the two P1 fixes on the cfg-dual `__autumn_execute_upsert` SQLite arm and
//! the versioned `upsert_many` path:
//!
//! * **Bug 2 — tenant isolation (SECURITY):** the Postgres upsert's
//!   `ON CONFLICT (id) DO UPDATE … WHERE tenant_id = $current` predicate keeps an
//!   id owned by another tenant from being hijacked. The SQLite arm conflicts on
//!   `id` only and its DO UPDATE set-clause writes `tenant_id`, so without the
//!   SAME `WHERE tenant_id = <current>` predicate, upserting another tenant's id
//!   would MOVE that row into the caller's tenant. This test proves the fix:
//!   `upsert_many` under tenant B with tenant A's id leaves tenant A's row
//!   completely untouched (no cross-tenant leak) and creates nothing under B.
//! * **Bug 2 — optimistic lock guard:** the SQLite DO UPDATE preserves
//!   `WHERE lock_version = excluded.lock_version`, so a stale-version upsert is
//!   left untouched, dropped from `RETURNING`, and surfaces as a 409 Conflict —
//!   matching Postgres exactly.
//!
//! Bug 1 (the versioned advisory lock `SELECT pg_advisory_xact_lock($1)` being
//! cfg-gated to Postgres-only, so it is skipped on SQLite's single-writer engine)
//! is pinned by the macro-expansion test
//! `repository_macro_versioned_upsert_many_gates_advisory_lock_to_postgres_only`
//! in `autumn-macros`. A full `versioned = true` (version-history) repository
//! cannot run end-to-end on SQLite yet because the version-history writer emits a
//! Postgres-only `$7::jsonb` cast — a separate unported construct outside these
//! two P1s — so the runtime lock-conflict behaviour is proven here with the
//! optimistic-lock (`#[lock_version]`) shape, which does not require version
//! history or the advisory lock.
//!
//! Uses an in-memory shared-cache `SQLite` database — no Docker.
//!
//! Only meaningful under `--features sqlite`; the file is
//! `#![cfg(feature = "sqlite")]` so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_upsert_isolation`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::tenancy::with_tenant;

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

mod schema {
    autumn_web::reexports::diesel::table! {
        iso_notes (id) {
            id -> Int8,
            content -> Text,
            tenant_id -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        iso_lock_notes (id) {
            id -> Int8,
            content -> Text,
            tenant_id -> Text,
            lock_version -> Int8,
        }
    }

    autumn_web::reexports::diesel::table! {
        plain_lock_notes (id) {
            id -> Int8,
            content -> Text,
            lock_version -> Int8,
        }
    }
}

use schema::{iso_lock_notes, iso_notes, plain_lock_notes};

// Tenant-scoped, no optimistic lock.
#[autumn_web::model(table = "iso_notes")]
pub struct IsoNote {
    #[id]
    pub id: i64,
    pub content: String,
    #[default]
    pub tenant_id: String,
}

#[autumn_web::repository(IsoNote, table = "iso_notes", tenant_scoped)]
pub trait IsoNoteRepository {}

// Tenant-scoped + optimistic lock (`has_lock`), NOT `versioned = true` (no
// version history / advisory lock), so it compiles and runs on SQLite.
#[autumn_web::model(table = "iso_lock_notes")]
pub struct IsoLockNote {
    #[id]
    pub id: i64,
    pub content: String,
    #[default]
    pub tenant_id: String,
    #[lock_version]
    pub lock_version: i64,
}

#[autumn_web::repository(IsoLockNote, table = "iso_lock_notes", tenant_scoped)]
pub trait IsoLockNoteRepository {}

// Plain optimistic lock, no tenant.
#[autumn_web::model(table = "plain_lock_notes")]
pub struct PlainLockNote {
    #[id]
    pub id: i64,
    pub content: String,
    #[lock_version]
    pub lock_version: i64,
}

#[autumn_web::repository(PlainLockNote, table = "plain_lock_notes")]
pub trait PlainLockNoteRepository {}

async fn boot_pool(db_name: &str) -> SqlitePool {
    // A shared-cache in-memory database so every pooled checkout observes the
    // same schema (a bare `:memory:` target is private per connection).
    let config = DatabaseConfig {
        url: Some(format!("sqlite://file:{db_name}?mode=memory&cache=shared")),
        primary_pool_size: Some(1),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds via build_sqlite_pool")
        .expect("a url is configured");

    {
        let mut conn = pool.get().await.expect("checkout a sqlite connection");
        diesel::sql_query(
            "CREATE TABLE iso_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 content TEXT NOT NULL, \
                 tenant_id TEXT NOT NULL\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create iso_notes table");
        diesel::sql_query(
            "CREATE TABLE iso_lock_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 content TEXT NOT NULL, \
                 tenant_id TEXT NOT NULL, \
                 lock_version BIGINT NOT NULL DEFAULT 1\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create iso_lock_notes table");
        diesel::sql_query(
            "CREATE TABLE plain_lock_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 content TEXT NOT NULL, \
                 lock_version BIGINT NOT NULL DEFAULT 1\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create plain_lock_notes table");
    }

    pool
}

/// SECURITY (Bug 2): a tenant-scoped `upsert_many` called under tenant B with an
/// id that belongs to tenant A must leave tenant A's row completely untouched —
/// the DO UPDATE `WHERE tenant_id = <current>` predicate never matches, so the
/// row is neither returned nor moved into tenant B. This is the no-cross-tenant-
/// leak assertion; without the SQLite tenant predicate the row's `tenant_id`
/// would be rewritten to "tenant-b".
#[tokio::test]
async fn tenant_scoped_upsert_many_never_leaks_cross_tenant_row_on_sqlite() {
    let pool = boot_pool("iso_upsert_tenant").await;
    let repo = PgIsoNoteRepository::with_pool_untracked(pool);

    // 1. Create a row owned by tenant-a.
    let record_a = with_tenant("tenant-a".to_string(), async {
        repo.save(&NewIsoNote {
            content: "owned-by-a".to_string(),
        })
        .await
        .expect("save under tenant-a")
    })
    .await;
    assert_eq!(record_a.tenant_id, "tenant-a");

    // 2. Under tenant-b, attempt to upsert tenant-a's id with new content.
    let upserted = with_tenant("tenant-b".to_string(), async {
        let mut hijack = record_a.clone();
        hijack.content = "hijacked-by-b".to_string();
        repo.upsert_many(&[hijack]).await
    })
    .await
    .expect("cross-tenant upsert must silently filter (Ok, not Err)");

    // The cross-tenant row is NOT in the result and nothing new was created.
    assert!(
        upserted.is_empty(),
        "no in-scope rows existed for tenant-b, so nothing should be upserted, got: {upserted:?}"
    );

    // 3. SECURITY: tenant-a's row is byte-for-byte untouched — content unchanged
    //    AND still owned by tenant-a (never moved into tenant-b).
    let a_after = repo
        .across_tenants()
        .find_by_id(record_a.id)
        .await
        .expect("query tenant-a row")
        .expect("tenant-a row still exists");
    assert_eq!(
        a_after.content, "owned-by-a",
        "tenant-a row content must be unchanged (no cross-tenant hijack)"
    );
    assert_eq!(
        a_after.tenant_id, "tenant-a",
        "tenant-a row must NOT be moved into tenant-b (no cross-tenant leak)"
    );

    // And tenant-b genuinely has nothing.
    let b_rows = with_tenant("tenant-b".to_string(), async {
        repo.find_all().await.expect("list tenant-b rows")
    })
    .await;
    assert!(
        b_rows.is_empty(),
        "tenant-b must own no rows after the filtered upsert, got: {b_rows:?}"
    );
}

/// Tenant-scoped + optimistic lock: proves the SQLite DO UPDATE carries BOTH the
/// tenant predicate (cross-tenant silent, no leak) AND the
/// `lock_version = excluded.lock_version` guard (same-tenant stale → 409),
/// matching Postgres exactly.
#[tokio::test]
async fn tenant_scoped_versioned_upsert_many_isolates_and_conflicts_on_sqlite() {
    let pool = boot_pool("iso_upsert_tenant_lock").await;
    let repo = PgIsoLockNoteRepository::with_pool_untracked(pool);

    // 1. Create a versioned row for tenant-a (lock_version defaults to 1).
    let record_a = with_tenant("tenant-a".to_string(), async {
        repo.save(&NewIsoLockNote {
            content: "a-v1".to_string(),
        })
        .await
        .expect("save under tenant-a")
    })
    .await;
    assert_eq!(record_a.lock_version, 1);
    assert_eq!(record_a.tenant_id, "tenant-a");

    // 2. Cross-tenant upsert under tenant-b is silently filtered, row untouched.
    let upserted = with_tenant("tenant-b".to_string(), async {
        let mut hijack = record_a.clone();
        hijack.content = "hijacked-by-b".to_string();
        repo.upsert_many(&[hijack]).await
    })
    .await
    .expect("cross-tenant versioned upsert must silently filter (Ok, not Err)");
    assert!(
        upserted.is_empty(),
        "cross-tenant versioned upsert must return nothing, got: {upserted:?}"
    );

    let a_after = repo
        .across_tenants()
        .find_by_id(record_a.id)
        .await
        .expect("query tenant-a row")
        .expect("tenant-a row still exists");
    assert_eq!(
        a_after.content, "a-v1",
        "content untouched by cross-tenant upsert"
    );
    assert_eq!(a_after.tenant_id, "tenant-a", "row not moved into tenant-b");
    assert_eq!(
        a_after.lock_version, 1,
        "lock version untouched by cross-tenant upsert"
    );

    // 3. A valid same-tenant upsert bumps the version.
    let bumped = with_tenant("tenant-a".to_string(), async {
        let mut valid = record_a.clone();
        valid.content = "a-v2".to_string();
        repo.upsert_many(&[valid])
            .await
            .expect("valid same-tenant upsert")
    })
    .await;
    assert_eq!(bumped.len(), 1);
    assert_eq!(bumped[0].content, "a-v2");
    assert_eq!(bumped[0].lock_version, 2, "DB increments the lock version");

    // 4. A STALE same-tenant upsert (still lock_version = 1) must fail loudly 409.
    let stale_res = with_tenant("tenant-a".to_string(), async {
        let mut stale = record_a.clone(); // lock_version = 1, DB now at 2
        stale.content = "a-stale".to_string();
        repo.upsert_many(&[stale]).await
    })
    .await;
    assert!(
        stale_res.is_err(),
        "stale same-tenant optimistic-lock upsert must fail loudly, but it succeeded"
    );
    let err_str = stale_res.unwrap_err().to_string();
    assert!(
        err_str.contains("onflict"),
        "expected a conflict error for a stale lock version, got: {err_str}"
    );

    // The stale write did not land: DB is still at v2 with content "a-v2".
    let final_a = repo
        .across_tenants()
        .find_by_id(record_a.id)
        .await
        .expect("query tenant-a row")
        .expect("tenant-a row still exists");
    assert_eq!(final_a.content, "a-v2");
    assert_eq!(final_a.lock_version, 2);
}

/// Plain (non-tenant) optimistic-lock `upsert_many` runs on SQLite: a valid
/// upsert bumps the version, and a stale one yields the 409 conflict — matching
/// Postgres.
#[tokio::test]
async fn plain_versioned_upsert_many_succeeds_and_conflicts_on_sqlite() {
    let pool = boot_pool("iso_upsert_plain_lock").await;
    let repo = PgPlainLockNoteRepository::with_pool_untracked(pool);

    let inserted = repo
        .save_many(&[NewPlainLockNote {
            content: "lock-a".to_string(),
        }])
        .await
        .expect("save_many inserts a versioned row");
    assert_eq!(inserted.len(), 1);
    let original = inserted[0].clone();
    assert_eq!(original.lock_version, 1);

    // Valid upsert with the current version succeeds and bumps the version.
    let mut to_upsert = original.clone();
    to_upsert.content = "lock-a-updated".to_string();
    let upserted = repo
        .upsert_many(&[to_upsert])
        .await
        .expect("valid versioned upsert_many must succeed on sqlite");
    assert_eq!(upserted.len(), 1);
    assert_eq!(upserted[0].content, "lock-a-updated");
    assert_eq!(
        upserted[0].lock_version, 2,
        "DB increments the lock version"
    );

    // Stale upsert (lock_version = 1, DB now at 2) must fail loudly with 409.
    let mut stale = original.clone();
    stale.content = "lock-a-stale".to_string();
    let stale_res = repo.upsert_many(&[stale]).await;
    assert!(
        stale_res.is_err(),
        "stale optimistic-lock upsert must fail loudly on sqlite, but it succeeded"
    );
    let err_str = stale_res.unwrap_err().to_string();
    assert!(
        err_str.contains("onflict"),
        "expected a conflict error for a stale lock version, got: {err_str}"
    );
}
