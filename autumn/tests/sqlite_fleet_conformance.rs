//! The fleet conformance kit, run against the `SQLite` engine with a
//! replicated fleet, as a plugin runs it against its own engine.
//!
//! Run: `cargo test -p autumn-web --features "sqlite,test-support" --test sqlite_fleet_conformance`.

#![cfg(feature = "sqlite")]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::config::{DatabaseConfig, DatabaseFleetConfig};
use autumn_web::db::fleet::{DatabaseFleet, FleetError, SqliteEngine};
use autumn_web::db::fleet_replication::FleetReplication;
use autumn_web::db::fleet_tombstones::FleetTombstones;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::fleet_layout::FleetDbKey;
use autumn_web::plugin_conformance::fleet::{FleetFixture, run_fleet_conformance};
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::replication::{FileDestination, ReplicaDestination, ReplicationSettings};
use diesel::sql_types::Text;
use diesel_async::pooled_connection::deadpool::Pool;
use diesel_async::{RunQueryDsl as _, SimpleAsyncConnection as _};

/// The `SQLite` fixture: values in one table, replicas in a local
/// directory, tombstones on a control database.
struct SqliteFixture;

#[derive(diesel::QueryableByName)]
struct Value {
    #[diesel(sql_type = Text)]
    value: String,
}

fn control_pool(dir: &Path) -> Pool<RuntimeConnection> {
    create_pool(&DatabaseConfig {
        url: Some(format!("sqlite://{}", dir.join("control.db").display())),
        ..Default::default()
    })
    .expect("control pool builds")
    .expect("url configured")
}

impl FleetFixture for SqliteFixture {
    type Engine = SqliteEngine;

    async fn fleet(&self, config: DatabaseFleetConfig) -> Result<DatabaseFleet, String> {
        let scratch = Path::new(&config.root)
            .parent()
            .ok_or("the root has no parent")?
            .to_path_buf();
        let fleet = DatabaseFleet::builder(config)
            .build()
            .map_err(|e| e.to_string())?;
        let destination: Arc<dyn ReplicaDestination> =
            Arc::new(FileDestination::new(scratch.join("replicas")).map_err(|e| e.to_string())?);
        let clock = Arc::new(autumn_web::time::SystemClock);
        let tombstones = Arc::new(FleetTombstones::new(
            control_pool(&scratch),
            Arc::clone(&clock) as _,
        ));
        let replication = Arc::new(
            FleetReplication::new(
                destination,
                ReplicationSettings {
                    database_path: std::path::PathBuf::new(),
                    root: "app/kit".to_owned(),
                    sync_interval: Duration::from_secs(1),
                    snapshot_interval: Duration::from_secs(3600),
                    max_wal_bytes: 4 * 1024 * 1024,
                    retention: Duration::from_hours(24),
                    verify_interval: None,
                },
                Duration::from_secs(30),
                false,
                clock,
            )
            .with_tombstones(tombstones),
        );
        fleet
            .install_replication(replication)
            .map_err(|_| "replication is installed twice".to_owned())?;
        Ok(fleet)
    }

    async fn write(&self, handle: &Pool<RuntimeConnection>, value: &str) -> Result<(), String> {
        let mut conn = handle.get().await.map_err(|e| e.to_string())?;
        conn.batch_execute("CREATE TABLE IF NOT EXISTS kit_values (value TEXT NOT NULL)")
            .await
            .map_err(|e| e.to_string())?;
        diesel::sql_query("INSERT INTO kit_values (value) VALUES (?)")
            .bind::<Text, _>(value)
            .execute(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn read(&self, handle: &Pool<RuntimeConnection>) -> Result<Vec<String>, String> {
        let mut conn = handle.get().await.map_err(|e| e.to_string())?;
        conn.batch_execute("CREATE TABLE IF NOT EXISTS kit_values (value TEXT NOT NULL)")
            .await
            .map_err(|e| e.to_string())?;
        let rows: Vec<Value> = diesel::sql_query("SELECT value FROM kit_values ORDER BY rowid")
            .load(&mut *conn)
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows.into_iter().map(|row| row.value).collect())
    }

    async fn restore(
        &self,
        fleet: &DatabaseFleet,
        key: &FleetDbKey,
    ) -> Option<Result<(), FleetError>> {
        Some(fleet.restore(key, None).await.map(|_| ()))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sqlite_engine_passes_the_fleet_conformance_kit() {
    let tmp = tempfile::tempdir().unwrap();
    let report = run_fleet_conformance("sqlite", &SqliteFixture, tmp.path()).await;
    assert!(report.passed(), "{}", report.to_text_report());
    for check in &report.checks {
        // Continuous shipping ships changes, not files.
        let expected = if check.name == "fleet-unchanged-files" {
            autumn_web::plugin_conformance::CheckStatus::Skip
        } else {
            autumn_web::plugin_conformance::CheckStatus::Pass
        };
        assert_eq!(check.status, expected, "{}: {}", check.name, check.message);
    }
    // Every check of the kit ran, the fenced ship too.
    assert!(
        report
            .checks
            .iter()
            .any(|check| check.name == "fleet-fenced-ship"),
        "{}",
        report.to_text_report()
    );
}

/// WAL shipping is the `continuous` mode. Its RPO is the sync interval, and
/// `ship_now` ships one tick through the fence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sqlite_engine_ships_continuously() {
    let tmp = tempfile::tempdir().unwrap();
    let config = DatabaseFleetConfig {
        mode: autumn_web::fleet_layout::FleetMode::Tenant,
        root: tmp.path().join("fleet").display().to_string(),
        path: None,
        max_open: 4,
        max_resident_bytes: 0,
        min_residency: Duration::ZERO,
        max_open_policy: autumn_web::config::FleetMaxOpenPolicy::Soft,
        pool_size: 2,
        create_on_demand: Some(true),
        idle_close_secs: 0,
        restore_missing: false,
        ownership: None,
        replica_retention_after_delete: Duration::ZERO,
        restore_check_sample: 0,
        replication_workers: 1,
        mmap_size: 0,
        cache_size_kib: 0,
    };
    let fleet = SqliteFixture.fleet(config).await.unwrap();
    let stats = fleet.stats();
    assert_eq!(stats.ship_mode, "continuous");
    assert_eq!(stats.rpo_secs, Some(1));
    let key = fleet.key_for("acme").unwrap();
    let db = fleet.open(&key).await.unwrap();
    SqliteFixture.write(db.handle(), "plan").await.unwrap();
    let report = fleet.ship_now(&key).await.unwrap();
    assert!(report.shipped >= 1, "{report:?}");
    assert_eq!(report.generation, None);
    drop(db);
    fleet.close_all().await;
}
