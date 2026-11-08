use super::{FleetFixture, run_fleet_conformance};
use crate::config::DatabaseFleetConfig;
use crate::db::fleet::DatabaseFleet;
use crate::db::fleet::test_engine::{TextEngine, TextHandle};
use crate::plugin_conformance::CheckStatus;

/// The kit fixture of the test file engine.
#[derive(Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each flag is one fixture choice"
)]
struct TextFixture {
    bytes: u64,
    skip_identity: bool,
    skip_epoch: bool,
    keep_on_remove: bool,
    /// Ship in `Generations` mode, with a replica and tombstones.
    generations: bool,
    /// Each snapshot differs, also with no write.
    noisy_snapshot: bool,
}

impl FleetFixture for TextFixture {
    type Engine = TextEngine;

    async fn fleet(
        &self,
        config: DatabaseFleetConfig,
    ) -> Result<DatabaseFleet<TextEngine>, String> {
        let engine = TextEngine {
            skip_identity: self.skip_identity,
            skip_epoch: self.skip_epoch,
            keep_on_remove: self.keep_on_remove,
            noisy_snapshot: self.noisy_snapshot,
            every: self
                .generations
                .then_some(std::time::Duration::from_hours(1)),
            ..TextEngine::new(self.bytes)
        };
        let scratch = std::path::Path::new(&config.root)
            .parent()
            .ok_or("the root has no parent")?
            .to_path_buf();
        let fleet = DatabaseFleet::with_engine(&config, engine).map_err(|e| e.to_string())?;
        #[cfg(feature = "sqlite")]
        if self.generations {
            replicate(&fleet, &scratch)?;
        }
        let _ = scratch;
        Ok(fleet)
    }

    async fn write(&self, handle: &TextHandle, value: &str) -> Result<(), String> {
        TextEngine::write(handle, value)
    }

    async fn read(&self, handle: &TextHandle) -> Result<Vec<String>, String> {
        TextEngine::read(handle)
    }

    async fn restore(
        &self,
        fleet: &DatabaseFleet<TextEngine>,
        key: &crate::fleet_layout::FleetDbKey,
    ) -> Option<Result<(), crate::db::fleet::FleetError>> {
        if self.generations {
            Some(fleet.restore_latest(key).await)
        } else {
            None
        }
    }
}

/// A replica in `scratch/replicas`, and tombstones on `scratch/control.db`.
#[cfg(feature = "sqlite")]
fn replicate(fleet: &DatabaseFleet<TextEngine>, scratch: &std::path::Path) -> Result<(), String> {
    use std::sync::Arc;
    use std::time::Duration;

    let destination: Arc<dyn crate::replication::ReplicaDestination> = Arc::new(
        crate::replication::FileDestination::new(scratch.join("replicas"))
            .map_err(|e| e.to_string())?,
    );
    let clock: Arc<dyn crate::time::ClockSource> = Arc::new(crate::time::SystemClock);
    let control = crate::db::create_pool(&crate::config::DatabaseConfig {
        url: Some(format!("sqlite://{}", scratch.join("control.db").display())),
        ..Default::default()
    })
    .map_err(|e| e.to_string())?
    .ok_or("no control url")?;
    let tombstones = Arc::new(crate::db::fleet_tombstones::FleetTombstones::new(
        control,
        Arc::clone(&clock),
    ));
    let replication = Arc::new(
        crate::db::fleet_replication::FleetReplication::new(
            destination,
            crate::replication::ReplicationSettings {
                database_path: std::path::PathBuf::new(),
                root: "app/kit".to_owned(),
                sync_interval: Duration::from_secs(1),
                snapshot_interval: Duration::from_hours(1),
                max_wal_bytes: 4 * 1024 * 1024,
                retention: Duration::from_hours(1),
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
        .map_err(|_| "replication is installed twice".to_owned())
}

fn status(report: &crate::plugin_conformance::ConformanceReport, name: &str) -> CheckStatus {
    report
        .checks
        .iter()
        .find(|check| check.name == name)
        .unwrap_or_else(|| panic!("no check {name}: {}", report.to_text_report()))
        .status
        .clone()
}

#[tokio::test]
async fn a_correct_engine_passes() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = TextFixture {
        bytes: 100,
        ..TextFixture::default()
    };
    let report = run_fleet_conformance("text", &fixture, tmp.path()).await;
    assert!(report.passed(), "{}", report.to_text_report());
    for name in [
        "fleet-identity",
        "fleet-epoch",
        "fleet-reopen",
        "fleet-snapshot-restore",
        "fleet-budget",
    ] {
        assert_eq!(status(&report, name), CheckStatus::Pass, "{name}");
    }
    // No replica: the restore half of the delete check is skipped, and so
    // are the ship checks.
    assert_eq!(status(&report, "fleet-delete"), CheckStatus::Pass);
    assert_eq!(status(&report, "fleet-delete-restore"), CheckStatus::Skip);
    assert_eq!(status(&report, "fleet-fenced-ship"), CheckStatus::Skip);
    assert_eq!(status(&report, "fleet-unchanged-files"), CheckStatus::Skip);
    assert_eq!(status(&report, "fleet-epoch-stamp"), CheckStatus::Pass);
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_engine_in_generations_mode_passes_every_check() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = TextFixture {
        bytes: 100,
        generations: true,
        ..TextFixture::default()
    };
    let report = run_fleet_conformance("text", &fixture, tmp.path()).await;
    assert!(report.passed(), "{}", report.to_text_report());
    for check in &report.checks {
        assert_eq!(
            check.status,
            CheckStatus::Pass,
            "{}: {}",
            check.name,
            check.message
        );
    }
}

#[cfg(feature = "sqlite")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snapshot_that_changes_with_no_write_fails_the_unchanged_check() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = TextFixture {
        bytes: 100,
        generations: true,
        noisy_snapshot: true,
        ..TextFixture::default()
    };
    let report = run_fleet_conformance("text", &fixture, tmp.path()).await;
    assert_eq!(status(&report, "fleet-unchanged-files"), CheckStatus::Fail);
    assert_eq!(status(&report, "fleet-fenced-ship"), CheckStatus::Pass);
}

#[tokio::test]
async fn an_engine_that_skips_the_identity_check_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = TextFixture {
        bytes: 100,
        skip_identity: true,
        ..TextFixture::default()
    };
    let report = run_fleet_conformance("text", &fixture, tmp.path()).await;
    assert!(!report.passed());
    assert_eq!(status(&report, "fleet-identity"), CheckStatus::Fail);
}

#[tokio::test]
async fn an_engine_that_reports_no_memory_fails_the_budget_check() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = TextFixture::default();
    let report = run_fleet_conformance("text", &fixture, tmp.path()).await;
    assert_eq!(status(&report, "fleet-budget"), CheckStatus::Fail);
}

#[tokio::test]
async fn the_restore_hook_must_refuse_after_a_delete() {
    struct Restores(TextFixture);
    impl FleetFixture for Restores {
        type Engine = TextEngine;
        async fn fleet(
            &self,
            config: DatabaseFleetConfig,
        ) -> Result<DatabaseFleet<TextEngine>, String> {
            self.0.fleet(config).await
        }
        async fn write(&self, handle: &TextHandle, value: &str) -> Result<(), String> {
            self.0.write(handle, value).await
        }
        async fn read(&self, handle: &TextHandle) -> Result<Vec<String>, String> {
            self.0.read(handle).await
        }
        async fn restore(
            &self,
            _fleet: &DatabaseFleet<TextEngine>,
            _key: &crate::fleet_layout::FleetDbKey,
        ) -> Option<Result<(), crate::db::fleet::FleetError>> {
            // A restore that brings a deleted database back.
            Some(Ok(()))
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let fixture = Restores(TextFixture {
        bytes: 100,
        ..TextFixture::default()
    });
    let report = run_fleet_conformance("text", &fixture, tmp.path()).await;
    assert_eq!(status(&report, "fleet-delete-restore"), CheckStatus::Fail);
}

#[tokio::test]
async fn an_engine_that_ignores_the_epoch_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = TextFixture {
        bytes: 100,
        skip_epoch: true,
        ..TextFixture::default()
    };
    let report = run_fleet_conformance("text", &fixture, tmp.path()).await;
    assert_eq!(status(&report, "fleet-epoch"), CheckStatus::Fail);
}

#[tokio::test]
async fn an_engine_that_removes_nothing_fails_the_delete_check() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = TextFixture {
        bytes: 100,
        keep_on_remove: true,
        ..TextFixture::default()
    };
    let report = run_fleet_conformance("text", &fixture, tmp.path()).await;
    assert_eq!(status(&report, "fleet-delete"), CheckStatus::Fail);
}

#[test]
fn a_snapshot_file_outside_the_database_entry_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("snap");
    std::fs::create_dir_all(&dir).unwrap();
    let stray = dir.join("other.db");
    std::fs::write(&stray, "x").unwrap();
    let error = super::place(
        &[stray],
        &dir,
        &tmp.path().join("a.db"),
        &tmp.path().join("b.db"),
    )
    .unwrap_err();
    assert!(error.contains("every file must be `a.db`"), "{error}");
}

#[test]
fn a_snapshot_entry_takes_the_target_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("snap");
    std::fs::create_dir_all(dir.join("a.db")).unwrap();
    std::fs::write(dir.join("a.db").join("000001.sst"), "sst").unwrap();
    let target = tmp.path().join("out").join("b.db");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    super::place(
        &[dir.join("a.db").join("000001.sst")],
        &dir,
        &tmp.path().join("a.db"),
        &target,
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(target.join("000001.sst")).unwrap(),
        "sst"
    );
}
