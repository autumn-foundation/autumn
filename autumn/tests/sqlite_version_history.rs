//! End-to-end `#[repository(versioned = true)]` version-history proof on the
//! `SQLite` runtime backend (issue #1996).
//!
//! Before this slice a `versioned = true` repository could not run on `SQLite`:
//! the writer emitted a Postgres-only `$7::jsonb` cast, the reader used
//! `::bigint` / `::text` / `::timestamptz` / `changes::text` casts plus the
//! `Timestamptz` diesel type (which has no `FromSql<_, Sqlite>`), and the
//! `_autumn_version_history` DDL was Postgres-only (`BIGSERIAL` / `JSONB` /
//! `DEFAULT NOW()` / `ADD COLUMN IF NOT EXISTS`). This test drives the forked
//! writer + reader against a real in-memory `SQLite` database and asserts:
//!
//! * a create + update + delete each append one `_autumn_version_history` row,
//!   read back through `version_history()` with the correct op / actor / changes
//!   ordering;
//! * the `recorded_at` time-range filter (`VersionFilter::between`) selects and
//!   excludes rows correctly (the writer binds `recorded_at` explicitly so the
//!   TEXT range comparisons stay monotonic);
//! * **fail-closed tenant isolation (adversarial):** `version_history()` scoped
//!   to tenant B must NOT return tenant A's history rows.
//!
//! Uses an in-memory shared-cache `SQLite` database — no Docker. Only meaningful
//! under `--features sqlite`; the file is `#![cfg(feature = "sqlite")]` so a
//! default `cargo test` compiles it to an empty (passing) binary. Run:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_version_history`.
#![cfg(feature = "sqlite")]
#![allow(clippy::must_use_candidate, clippy::missing_const_for_fn)]

use autumn_web::config::DatabaseConfig;
use autumn_web::current::with_actor;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::hooks::Patch;
use autumn_web::reexports::{chrono, diesel, diesel_async};
use autumn_web::tenancy::with_tenant;
use autumn_web::version_history::{VersionFilter, VersionOp};

use chrono::Duration;
use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

// ── Plain (non-tenant) versioned repository ──────────────────────────────────

mod schema {
    autumn_web::reexports::diesel::table! {
        vh_notes (id) {
            id -> Int8,
            content -> Text,
        }
    }

    autumn_web::reexports::diesel::table! {
        vh_tenant_notes (id) {
            id -> Int8,
            content -> Text,
            tenant_id -> Text,
        }
    }
}

use schema::{vh_notes, vh_tenant_notes};

#[autumn_web::model(table = "vh_notes")]
#[derive(PartialEq, Eq)]
pub struct VhNote {
    #[id]
    pub id: i64,
    pub content: String,
}

#[autumn_web::repository(VhNote, table = "vh_notes", versioned = true)]
pub trait VhNoteRepository {}

// ── Tenant-scoped versioned repository (isolation adversary) ─────────────────

#[autumn_web::model(table = "vh_tenant_notes")]
#[derive(PartialEq, Eq)]
pub struct VhTenantNote {
    #[id]
    pub id: i64,
    pub content: String,
    #[default]
    pub tenant_id: String,
}

#[autumn_web::repository(
    VhTenantNote,
    table = "vh_tenant_notes",
    tenant_scoped,
    versioned = true
)]
pub trait VhTenantNoteRepository {}

/// The SQLite `_autumn_version_history` DDL — the exact fork shipped in
/// `autumn/version_history_migrations_sqlite/…/up.sql`. Applied by hand here so
/// the test proves the SQLite CREATE is valid and the writer/reader work against
/// it without booting the full migration harness.
const VERSION_HISTORY_DDL: &str = "CREATE TABLE IF NOT EXISTS _autumn_version_history (\
        id          INTEGER PRIMARY KEY, \
        table_name  TEXT    NOT NULL, \
        tenant_id   TEXT, \
        record_id   BIGINT  NOT NULL, \
        op          TEXT    NOT NULL CHECK (op IN ('insert', 'update', 'delete')), \
        actor       TEXT    NOT NULL DEFAULT 'system', \
        request_id  TEXT, \
        changes     TEXT    NOT NULL DEFAULT '[]', \
        recorded_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP\
    )";

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
            "CREATE TABLE vh_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 content TEXT NOT NULL\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create vh_notes table");
        diesel::sql_query(
            "CREATE TABLE vh_tenant_notes (\
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 content TEXT NOT NULL, \
                 tenant_id TEXT NOT NULL\
             )",
        )
        .execute(&mut *conn)
        .await
        .expect("create vh_tenant_notes table");
        diesel::sql_query(VERSION_HISTORY_DDL)
            .execute(&mut *conn)
            .await
            .expect("create _autumn_version_history (proves the SQLite DDL is valid)");
    }

    pool
}

/// A create + update + delete each append one history row, and `version_history()`
/// reads them back chronologically with the right op / actor / changes.
#[tokio::test]
async fn versioned_repository_records_and_reads_history_on_sqlite() {
    let pool = boot_pool("vh_basic").await;
    let repo = PgVhNoteRepository::with_pool_untracked(pool);

    // create → update → delete, all attributed to "alice".
    let id = with_actor("alice", async {
        let created = repo
            .save(&NewVhNote {
                content: "v1".to_string(),
            })
            .await
            .expect("insert");
        repo.update(
            created.id,
            &UpdateVhNote {
                content: Patch::Set("v2".to_string()),
            },
        )
        .await
        .expect("update");
        repo.delete_by_id(created.id).await.expect("delete");
        created.id
    })
    .await;

    let page = repo
        .version_history(id, VersionFilter::default())
        .await
        .expect("version_history reads back on sqlite");

    assert_eq!(page.total, 3, "create + update + delete = 3 history rows");
    assert_eq!(page.entries.len(), 3);

    // Chronological order (oldest first): insert, update, delete.
    let ops: Vec<VersionOp> = page.entries.iter().map(|e| e.op).collect();
    assert_eq!(
        ops,
        vec![VersionOp::Insert, VersionOp::Update, VersionOp::Delete],
        "entries must be oldest-first insert/update/delete"
    );

    // Actor attribution round-trips.
    for entry in &page.entries {
        assert_eq!(entry.actor, "alice", "each entry records the scoped actor");
        assert_eq!(entry.record_id, id);
        assert_eq!(entry.table_name, "vh_notes");
    }

    // The column-level diff (stored as JSON TEXT, not jsonb) round-trips: the
    // update entry carries a `content` change v1 → v2.
    let update_entry = page
        .entries
        .iter()
        .find(|e| e.op == VersionOp::Update)
        .expect("an update entry");
    let content_change = update_entry
        .changes
        .iter()
        .find(|c| c.column == "content")
        .expect("update diff includes the content column");
    assert_eq!(
        content_change.before.as_ref().and_then(|v| v.as_str()),
        Some("v1"),
        "update before-image is v1"
    );
    assert_eq!(
        content_change.after.as_ref().and_then(|v| v.as_str()),
        Some("v2"),
        "update after-image is v2"
    );

    // The insert entry records the created value.
    let insert_entry = page
        .entries
        .iter()
        .find(|e| e.op == VersionOp::Insert)
        .expect("an insert entry");
    assert!(
        !insert_entry.changes.is_empty(),
        "insert entry carries a non-empty column diff"
    );
}

/// The `recorded_at` TEXT range filter selects and excludes rows correctly — the
/// writer binds `recorded_at` explicitly so stored and filter values share one
/// encoding and the `>=` / `<=` TEXT comparisons stay monotonic.
#[tokio::test]
async fn version_history_time_range_filter_on_sqlite() {
    let pool = boot_pool("vh_time_range").await;
    let repo = PgVhNoteRepository::with_pool_untracked(pool);

    let created = repo
        .save(&NewVhNote {
            content: "only".to_string(),
        })
        .await
        .expect("insert");

    let now = chrono::Utc::now();

    // A window straddling now includes the insert.
    let wide = repo
        .version_history(
            created.id,
            VersionFilter::between(now - Duration::hours(1), now + Duration::hours(1)),
        )
        .await
        .expect("wide range read");
    assert_eq!(
        wide.total, 1,
        "the insert falls inside the straddling window"
    );
    assert_eq!(wide.entries.len(), 1);

    // A future-only window excludes it.
    let future = repo
        .version_history(
            created.id,
            VersionFilter::between(now + Duration::hours(1), now + Duration::hours(2)),
        )
        .await
        .expect("future range read");
    assert_eq!(
        future.total, 0,
        "a strictly-future window must exclude the insert"
    );
    assert!(future.entries.is_empty());
}

/// SECURITY (adversarial): version history is tenant-scoped by default, so
/// `version_history()` under tenant B must NOT return tenant A's rows. Fail-closed
/// tenant isolation is inviolable.
#[tokio::test]
async fn version_history_is_tenant_isolated_on_sqlite() {
    let pool = boot_pool("vh_tenant_iso").await;
    let repo = PgVhTenantNoteRepository::with_pool_untracked(pool);

    // Create a record (and its insert-history row) owned by tenant-a.
    let record_a = with_tenant("tenant-a".to_string(), async {
        repo.save(&NewVhTenantNote {
            content: "a-secret".to_string(),
        })
        .await
        .expect("save under tenant-a")
    })
    .await;
    assert_eq!(record_a.tenant_id, "tenant-a");

    // Under tenant-a the history is visible.
    let seen_by_a = with_tenant("tenant-a".to_string(), async {
        repo.version_history(record_a.id, VersionFilter::default())
            .await
            .expect("tenant-a reads its own history")
    })
    .await;
    assert_eq!(
        seen_by_a.total, 1,
        "tenant-a must see its own insert history row"
    );
    assert_eq!(seen_by_a.entries.len(), 1);
    assert_eq!(seen_by_a.entries[0].op, VersionOp::Insert);

    // ADVERSARIAL: under tenant-b the SAME record_id must resolve to zero rows —
    // the tenant predicate fails closed, never leaking tenant-a's history.
    let seen_by_b = with_tenant("tenant-b".to_string(), async {
        repo.version_history(record_a.id, VersionFilter::default())
            .await
            .expect("tenant-b query itself succeeds")
    })
    .await;
    assert_eq!(
        seen_by_b.total, 0,
        "SECURITY: tenant-b must NOT see tenant-a's version history (fail-closed isolation)"
    );
    assert!(
        seen_by_b.entries.is_empty(),
        "SECURITY: tenant-b must receive no history entries for tenant-a's record, got: {:?}",
        seen_by_b.entries
    );
}
