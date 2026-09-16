//! `FRAMEWORK_MIGRATIONS` applies on `SQLite` (issue #2699).
//!
//! `autumn/migrations` backed `api_tokens`, the job queue, feature flags,
//! experiments, the shard directory, and the ledger — but only for Postgres.
//! A `SQLite` app that registered `FRAMEWORK_MIGRATIONS` could not apply it.
//! The embedded set held Postgres-only DDL (`BIGSERIAL`, `JSONB`,
//! `TIMESTAMPTZ`, `NOW()`, `pg_notify` triggers). This is the CI-backed proof
//! that the `SQLite` fork (`autumn/migrations_sqlite`) now applies cleanly
//! through the real `MigrationHarness`, not a hand-copied DDL string.
//!
//! Only meaningful under `--features sqlite`. The file is
//! `#![cfg(feature = "sqlite")]`, so a default `cargo test` compiles it to an
//! empty (passing) binary. Run explicitly:
//! `cargo test -p autumn-web --features sqlite --test sqlite_framework_migrations`.
#![cfg(feature = "sqlite")]

use autumn_web::config::DatabaseConfig;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::migrate::{FRAMEWORK_MIGRATIONS, run_pending_sqlite};
use autumn_web::reexports::diesel;

use diesel_async::RunQueryDsl as _;
use diesel_async::pooled_connection::deadpool::Pool;

type SqlitePool = Pool<RuntimeConnection>;

#[derive(diesel::QueryableByName)]
struct TableName {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

/// Every table the control-plane schema creates on `SQLite`. Tables owned by
/// another self-managed bootstrap (jobs, job tracking) or moot under
/// `sqlite_sharding_unsupported_guard` (shard directory, shard map) are
/// deliberately absent: see `autumn/migrations_sqlite`'s shimmed migrations.
const EXPECTED_TABLES: &[&str] = &[
    "api_tokens",
    "autumn_repository_commit_hooks",
    "_autumn_version_history",
    "autumn_runtime_config_values",
    "autumn_runtime_config_changes",
    "autumn_feature_flags",
    "feature_flag_changes",
    "autumn_experiments",
    "autumn_experiment_assignments",
    "autumn_experiment_overrides",
    "autumn_experiment_changes",
    "autumn_migration_checksums",
    "_autumn_ledger_revisions",
    "_autumn_ledger_high_water",
    "_autumn_derivations",
];

/// Tables owned by another bootstrap and never created by
/// `FRAMEWORK_MIGRATIONS` on `SQLite` — a regression here would mean a shim
/// migration grew real DDL and now fights the owning module's own schema
/// setup (`job/sqlite.rs`, `job_tracking.rs`).
const SHIMMED_TABLES: &[&str] = &[
    "autumn_jobs",
    "autumn_job_tracking",
    "_autumn_shard_directory",
    "_autumn_shard_map",
];

#[tokio::test]
async fn framework_migrations_apply_on_sqlite() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let db_path = tmp.path().join("framework.db");
    let url = format!("sqlite://{}", db_path.display());

    let result =
        run_pending_sqlite(&url, FRAMEWORK_MIGRATIONS).expect("framework migrations apply");
    assert!(
        !result.applied.is_empty(),
        "at least one framework migration applied"
    );

    // Re-running is a no-op: every version is already recorded.
    let again = run_pending_sqlite(&url, FRAMEWORK_MIGRATIONS)
        .expect("re-running pending framework migrations is a no-op");
    assert!(
        again.applied.is_empty(),
        "second run applies nothing (got {:?})",
        again.applied
    );

    let config = DatabaseConfig {
        url: Some(url),
        ..Default::default()
    };
    let pool: SqlitePool = create_pool(&config)
        .expect("sqlite pool builds")
        .expect("a url is configured");
    let mut conn = pool.get().await.expect("checkout a sqlite connection");
    let rows: Vec<TableName> =
        diesel::sql_query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .load(&mut *conn)
            .await
            .expect("list tables");
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();

    for expected in EXPECTED_TABLES {
        assert!(
            names.contains(expected),
            "expected table `{expected}` was not created (got {names:?})"
        );
    }
    for shimmed in SHIMMED_TABLES {
        assert!(
            !names.contains(shimmed),
            "`{shimmed}` is owned by its own bootstrap and must not be \
             created by FRAMEWORK_MIGRATIONS (got {names:?})"
        );
    }
}
