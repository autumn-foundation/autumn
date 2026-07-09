//! Database-level integration tests for race-safe get-or-insert
//! (`find_or_create_by_*`, issue #1382).
//!
//! The seeded-table tests **require Docker** (testcontainers) and are
//! `#[ignore]`d by default. One non-ignored test proves the generated method
//! surface exists without a live database.
//!
//! Fixtures deliberately span every generated branch — plain, composite,
//! tenant-scoped, hooked (`before`/`after_create`), commit-hooks, versioned and
//! soft-delete — so `cargo test -p autumn --no-run --features db` type-checks
//! all of them even though the ignored tests can't run here.

#![cfg(feature = "db")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::AutumnResult;
use autumn_web::hooks::{MutationContext, MutationHooks};
use autumn_web::repository::ReadRoute;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

// ── Plain model (single unique lookup column) ─────────────────────────────────

diesel::table! {
    test_foc_records (id) {
        id -> Int8,
        slug -> Text,
        name -> Text,
    }
}

#[autumn_web::model(table = "test_foc_records")]
#[derive(PartialEq, Eq)]
pub struct FocRecord {
    #[id]
    pub id: i64,
    pub slug: String,
    pub name: String,
}

#[autumn_web::repository(FocRecord, table = "test_foc_records")]
pub trait FocRecordRepository {
    /// Race-safe get-or-insert on the unique `slug` column.
    fn find_or_create_by_slug(slug: String);
    /// Plain finder, for cross-checking row equality.
    fn find_by_slug(slug: String) -> Vec<FocRecord>;
}

// ── Composite unique lookup (a, b) ────────────────────────────────────────────

diesel::table! {
    test_foc_composite (id) {
        id -> Int8,
        a -> Text,
        b -> Text,
        note -> Text,
    }
}

#[autumn_web::model(table = "test_foc_composite")]
#[derive(PartialEq, Eq)]
pub struct FocComposite {
    #[id]
    pub id: i64,
    pub a: String,
    pub b: String,
    pub note: String,
}

#[autumn_web::repository(FocComposite, table = "test_foc_composite")]
pub trait FocCompositeRepository {
    /// Composite lookup backed by a `UNIQUE (a, b)` index.
    fn find_or_create_by_a_and_b(a: String, b: String);
}

// ── Tenant-scoped model ───────────────────────────────────────────────────────

diesel::table! {
    test_foc_tenant_records (id) {
        id -> Int8,
        slug -> Text,
        name -> Text,
        tenant_id -> Text,
    }
}

#[autumn_web::model(table = "test_foc_tenant_records")]
#[derive(PartialEq, Eq)]
pub struct FocTenantRecord {
    #[id]
    pub id: i64,
    pub slug: String,
    pub name: String,
    #[default]
    pub tenant_id: String,
}

#[autumn_web::repository(FocTenantRecord, table = "test_foc_tenant_records", tenant_scoped)]
pub trait FocTenantRecordRepository {
    fn find_or_create_by_slug(slug: String);
}

// ── Hooked model (before/after_create fire on the created path only) ───────────

static FOC_HOOK_CREATES: AtomicUsize = AtomicUsize::new(0);

diesel::table! {
    test_foc_hooked_records (id) {
        id -> Int8,
        slug -> Text,
        name -> Text,
    }
}

#[autumn_web::model(table = "test_foc_hooked_records")]
#[derive(PartialEq, Eq)]
pub struct FocHookedRecord {
    #[id]
    pub id: i64,
    pub slug: String,
    pub name: String,
}

#[derive(Clone, Default)]
pub struct FocHookedHooks;

impl MutationHooks for FocHookedHooks {
    type Model = FocHookedRecord;
    type NewModel = NewFocHookedRecord;
    type UpdateModel = UpdateFocHookedRecord;

    async fn after_create(
        &self,
        _ctx: &mut MutationContext,
        _record: &FocHookedRecord,
    ) -> AutumnResult<()> {
        FOC_HOOK_CREATES.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[autumn_web::repository(FocHookedRecord, table = "test_foc_hooked_records", hooks = FocHookedHooks)]
pub trait FocHookedRecordRepository {
    fn find_or_create_by_slug(slug: String);
}

// ── Commit-hooks model (durable after_create_commit queue) — compile-only ─────

diesel::table! {
    test_foc_commit_records (id) {
        id -> Int8,
        slug -> Text,
        name -> Text,
    }
}

#[autumn_web::model(table = "test_foc_commit_records")]
pub struct FocCommitRecord {
    #[id]
    pub id: i64,
    pub slug: String,
    pub name: String,
}

#[derive(Clone, Default)]
pub struct FocCommitHooks;

impl MutationHooks for FocCommitHooks {
    type Model = FocCommitRecord;
    type NewModel = NewFocCommitRecord;
    type UpdateModel = UpdateFocCommitRecord;
}

#[autumn_web::repository(
    FocCommitRecord,
    table = "test_foc_commit_records",
    hooks = FocCommitHooks,
    commit_hooks = true,
    no_upsert_trait
)]
pub trait FocCommitRecordRepository {
    fn find_or_create_by_slug(slug: String);
}

// ── Versioned model (no hooks) — compile-only ─────────────────────────────────

diesel::table! {
    test_foc_versioned_records (id) {
        id -> Int8,
        slug -> Text,
        name -> Text,
    }
}

#[autumn_web::model(table = "test_foc_versioned_records")]
pub struct FocVersionedRecord {
    #[id]
    pub id: i64,
    pub slug: String,
    pub name: String,
}

#[autumn_web::repository(
    FocVersionedRecord,
    table = "test_foc_versioned_records",
    versioned = true
)]
pub trait FocVersionedRecordRepository {
    fn find_or_create_by_slug(slug: String);
}

// ── Soft-delete model — compile-only ──────────────────────────────────────────

diesel::table! {
    test_foc_soft_records (id) {
        id -> Int8,
        slug -> Text,
        name -> Text,
        deleted_at -> Nullable<Timestamp>,
    }
}

#[autumn_web::model(table = "test_foc_soft_records")]
pub struct FocSoftRecord {
    #[id]
    pub id: i64,
    pub slug: String,
    pub name: String,
    #[default]
    pub deleted_at: Option<chrono::NaiveDateTime>,
}

#[autumn_web::repository(FocSoftRecord, table = "test_foc_soft_records", soft_delete)]
pub trait FocSoftRecordRepository {
    fn find_or_create_by_slug(slug: String);
}

// ── Setup & helpers ───────────────────────────────────────────────────────────

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
    // Sized to comfortably exceed the 10 concurrent race callers below.
    let pool = Pool::builder(manager).max_size(20).build().expect("pool");

    let mut conn = pool.get().await.expect("conn");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_foc_records \
         (id BIGSERIAL PRIMARY KEY, slug TEXT NOT NULL UNIQUE, name TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_foc_records");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_foc_composite \
         (id BIGSERIAL PRIMARY KEY, a TEXT NOT NULL, b TEXT NOT NULL, note TEXT NOT NULL, \
          CONSTRAINT test_foc_composite_a_b_key UNIQUE (a, b))",
    )
    .execute(&mut conn)
    .await
    .expect("create test_foc_composite");
    diesel::sql_query(
        "CREATE TABLE IF NOT EXISTS test_foc_hooked_records \
         (id BIGSERIAL PRIMARY KEY, slug TEXT NOT NULL UNIQUE, name TEXT NOT NULL)",
    )
    .execute(&mut conn)
    .await
    .expect("create test_foc_hooked_records");

    (pool, container)
}

const fn build_repo(pool: Pool<AsyncPgConnection>) -> PgFocRecordRepository {
    PgFocRecordRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

const fn build_composite_repo(pool: Pool<AsyncPgConnection>) -> PgFocCompositeRepository {
    PgFocCompositeRepository {
        pool,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

const fn build_hooked_repo(pool: Pool<AsyncPgConnection>) -> PgFocHookedRecordRepository {
    PgFocHookedRecordRepository {
        pool,
        hooks: FocHookedHooks,
        __autumn_read_route: ReadRoute::Primary,
        __autumn_statement_timeout_ms: 0,
        __autumn_slow_threshold: std::time::Duration::from_millis(500),
        __autumn_route: None,
    }
}

// ── Non-ignored: the generated surface exists without a live database ──────────

/// The generated inherent methods type-check and can be named as function
/// items (proving the codegen ran) even where Docker is unavailable. This is
/// the #1592-style non-ignored guard.
#[test]
fn find_or_create_by_methods_are_generated() {
    // Consuming each generated method as a function item proves the codegen ran
    // for every branch (plain, composite, tenant, hooked, commit-hooks,
    // versioned, soft-delete) without needing a live database.
    fn assert_is_fn<F>(_f: F) {}
    assert_is_fn(PgFocRecordRepository::find_or_create_by_slug);
    assert_is_fn(PgFocCompositeRepository::find_or_create_by_a_and_b);
    assert_is_fn(PgFocTenantRecordRepository::find_or_create_by_slug);
    assert_is_fn(PgFocHookedRecordRepository::find_or_create_by_slug);
    assert_is_fn(PgFocCommitRecordRepository::find_or_create_by_slug);
    assert_is_fn(PgFocVersionedRecordRepository::find_or_create_by_slug);
    assert_is_fn(PgFocSoftRecordRepository::find_or_create_by_slug);
}

// ── Tests (require Docker) ────────────────────────────────────────────────────

/// AC1: creates when absent (`created == true`), and a subsequent call with the
/// same key finds it (`created == false`) — same row, no duplicate insert.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_or_create_by_creates_then_finds() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    let new = NewFocRecord {
        slug: "rust".to_string(),
        name: "Rustaceans".to_string(),
    };

    let (created, was_created) = repo
        .find_or_create_by_slug("rust".to_string(), &new)
        .await
        .expect("first call succeeds");
    assert!(was_created, "first call must insert the row");
    assert_eq!(created.slug, "rust");

    // AC3: second call finds the existing row and reports created=false.
    let new2 = NewFocRecord {
        slug: "rust".to_string(),
        name: "SHOULD NOT BE INSERTED".to_string(),
    };
    let (found, was_created2) = repo
        .find_or_create_by_slug("rust".to_string(), &new2)
        .await
        .expect("second call succeeds");
    assert!(!was_created2, "second call must find, not insert");
    assert_eq!(found.id, created.id, "same row returned");
    assert_eq!(
        found.name, "Rustaceans",
        "existing row wins; new value ignored"
    );

    // Exactly one row exists for the key.
    let all = repo.find_by_slug("rust".to_string()).await.unwrap();
    assert_eq!(all.len(), 1);
}

/// AC3: when a matching row already exists, the returned model equals the
/// existing row (byte-for-byte via `PartialEq`) and `created == false`.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_or_create_by_returns_existing_row_unchanged() {
    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    let seeded = repo
        .save(&NewFocRecord {
            slug: "go".to_string(),
            name: "Gophers".to_string(),
        })
        .await
        .expect("seed row");

    let (found, was_created) = repo
        .find_or_create_by_slug(
            "go".to_string(),
            &NewFocRecord {
                slug: "go".to_string(),
                name: "ignored".to_string(),
            },
        )
        .await
        .expect("find succeeds");

    assert!(!was_created);
    assert_eq!(found, seeded, "returned model equals the existing row");
}

/// AC2: race safety under ≥10 concurrent callers for the same key. Exactly one
/// row exists afterward, exactly one caller reports `created == true`, and no
/// unique-violation (23505) escapes to any caller.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_or_create_by_is_race_safe_under_concurrency() {
    const CALLERS: usize = 16;

    let (pool, _container) = setup_pool().await;
    let repo = build_repo(pool);

    let mut handles = Vec::new();
    for i in 0..CALLERS {
        let repo = repo.clone();
        handles.push(tokio::spawn(async move {
            repo.find_or_create_by_slug(
                "contended".to_string(),
                &NewFocRecord {
                    slug: "contended".to_string(),
                    name: format!("caller-{i}"),
                },
            )
            .await
        }));
    }

    let mut created_count = 0usize;
    let mut ids = Vec::new();
    for h in handles {
        // No caller may observe an error — 23505 must never escape.
        let (row, created) = h
            .await
            .expect("task did not panic")
            .expect("no unique-violation (23505) may surface to any caller");
        if created {
            created_count += 1;
        }
        ids.push(row.id);
    }

    assert_eq!(
        created_count, 1,
        "exactly one caller may report created=true"
    );
    assert!(
        ids.windows(2).all(|w| w[0] == w[1]),
        "every caller observed the same row id: {ids:?}"
    );

    let all = repo.find_by_slug("contended".to_string()).await.unwrap();
    assert_eq!(all.len(), 1, "exactly one row exists after the race");
}

/// AC4/AC5: on a hooked repository, `after_create` fires only on the created
/// path (`created == true`) and never on the found path.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_or_create_by_fires_after_create_only_when_created() {
    let (pool, _container) = setup_pool().await;
    let repo = build_hooked_repo(pool);

    FOC_HOOK_CREATES.store(0, Ordering::SeqCst);

    let new = NewFocHookedRecord {
        slug: "hooked".to_string(),
        name: "first".to_string(),
    };
    let (_row, created) = repo
        .find_or_create_by_slug("hooked".to_string(), &new)
        .await
        .expect("create path");
    assert!(created);
    assert_eq!(
        AtomicUsize::load(&FOC_HOOK_CREATES, Ordering::SeqCst),
        1,
        "after_create fires once on the created path"
    );

    // Found path: hook must NOT fire again.
    let (_row2, created2) = repo
        .find_or_create_by_slug(
            "hooked".to_string(),
            &NewFocHookedRecord {
                slug: "hooked".to_string(),
                name: "second".to_string(),
            },
        )
        .await
        .expect("found path");
    assert!(!created2);
    assert_eq!(
        AtomicUsize::load(&FOC_HOOK_CREATES, Ordering::SeqCst),
        1,
        "after_create must NOT fire on the found path"
    );
}

/// AC1 for a composite unique key: `find_or_create_by_a_and_b` inserts once and
/// then finds the same row.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn find_or_create_by_composite_key() {
    let (pool, _container) = setup_pool().await;
    let repo = build_composite_repo(pool);

    let (row, created) = repo
        .find_or_create_by_a_and_b(
            "x".to_string(),
            "y".to_string(),
            &NewFocComposite {
                a: "x".to_string(),
                b: "y".to_string(),
                note: "first".to_string(),
            },
        )
        .await
        .expect("create");
    assert!(created);

    let (row2, created2) = repo
        .find_or_create_by_a_and_b(
            "x".to_string(),
            "y".to_string(),
            &NewFocComposite {
                a: "x".to_string(),
                b: "y".to_string(),
                note: "ignored".to_string(),
            },
        )
        .await
        .expect("find");
    assert!(!created2);
    assert_eq!(row.id, row2.id);
}
