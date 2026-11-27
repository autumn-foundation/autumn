//! Tests of the boot-time installer, on the test file engine.

use std::path::Path;
use std::time::Duration;

use super::{InstallReplicationError, ReplicationInstall, install_replication, replicated_state};
use crate::AppState;
use crate::config::{DatabaseFleetConfig, FleetOwnershipConfig};
use crate::db::fleet::test_engine::{TextEngine, Warnings};
use crate::db::fleet::{DatabaseFleet, UNREPLICATED_WARNING};
use crate::db::fleet_replication::FLEET_REPLICATION_INDICATOR;
use crate::fleet_layout::FleetMode;

const HOUR: Duration = Duration::from_hours(1);

fn config(root: &Path) -> DatabaseFleetConfig {
    DatabaseFleetConfig {
        mode: FleetMode::Tenant,
        root: root.display().to_string(),
        path: None,
        max_open: 8,
        max_resident_bytes: 0,
        min_residency: Duration::ZERO,
        max_open_policy: crate::config::FleetMaxOpenPolicy::Soft,
        pool_size: 1,
        create_on_demand: Some(true),
        idle_close_secs: 0,
        restore_missing: false,
        ownership: None,
        replica_retention_after_delete: Duration::ZERO,
        restore_check_sample: 0,
        replication_workers: 1,
        mmap_size: 0,
        cache_size_kib: 0,
    }
}

fn owned(root: &Path) -> DatabaseFleetConfig {
    DatabaseFleetConfig {
        ownership: Some(FleetOwnershipConfig {
            owner: "host-a".to_owned(),
            slots: Vec::new(),
            lease_secs: 30,
        }),
        ..config(root)
    }
}

/// A fleet of the text engine. `every`: `Generations` with this interval,
/// else `Rebuild`.
fn fleet(config: &DatabaseFleetConfig, every: Option<Duration>) -> DatabaseFleet<TextEngine> {
    let mut engine = TextEngine::new(100);
    engine.every = every;
    DatabaseFleet::with_engine(config, engine).unwrap()
}

fn stop(fleet: &DatabaseFleet<TextEngine>) {
    if let Some(replication) = fleet.replication() {
        replication.stop();
    }
}

#[tokio::test]
async fn the_installer_applies_the_fleet_section() {
    let tmp = tempfile::tempdir().unwrap();
    let state = replicated_state(tmp.path()).await;
    let config = DatabaseFleetConfig {
        restore_missing: true,
        replica_retention_after_delete: HOUR,
        restore_check_sample: 3,
        replication_workers: 7,
        ..owned(&tmp.path().join("fleet"))
    };
    let fleet = fleet(&config, Some(HOUR));

    let outcome = install_replication(&state, &fleet, &config).unwrap();

    assert_eq!(outcome, ReplicationInstall::Installed);
    let replication = fleet.replication().expect("the fleet replicates");
    assert!(replication.restores_missing());
    assert_eq!(replication.retention_after_delete(), HOUR);
    assert_eq!(replication.ship_workers(), 7);
    assert_eq!(replication.final_flush(), Duration::from_secs(5));
    assert!(
        replication.tombstones().is_some(),
        "on the control database"
    );
    assert_eq!(
        replication.restore_check().map(|(_, sample)| sample),
        state.capabilities().restore_check.then_some(3)
    );
    assert!(
        state
            .health_indicator_registry()
            .contains(FLEET_REPLICATION_INDICATOR)
    );
    stop(&fleet);
}

#[tokio::test]
async fn a_second_install_on_one_fleet_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let state = replicated_state(tmp.path()).await;
    let config = config(&tmp.path().join("fleet"));
    let fleet = fleet(&config, Some(HOUR));
    assert_eq!(
        install_replication(&state, &fleet, &config).unwrap(),
        ReplicationInstall::Installed
    );

    let again = install_replication(&state, &fleet, &config).unwrap_err();

    assert!(
        matches!(again, InstallReplicationError::AlreadyReplicates),
        "{again}"
    );
    stop(&fleet);
}

#[tokio::test]
async fn a_second_fleet_on_one_replica_root_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let state = replicated_state(tmp.path()).await;
    let first_config = config(&tmp.path().join("first"));
    let first = fleet(&first_config, Some(HOUR));
    assert_eq!(
        install_replication(&state, &first, &first_config).unwrap(),
        ReplicationInstall::Installed
    );
    let second_config = config(&tmp.path().join("second"));
    let second = fleet(&second_config, Some(HOUR));

    let error = install_replication(&state, &second, &second_config).unwrap_err();

    assert!(
        matches!(error, InstallReplicationError::RootInUse { .. }),
        "{error}"
    );
    assert!(second.replication().is_none(), "nothing installed");
    stop(&first);
}

#[tokio::test]
async fn a_second_fleet_on_the_same_root_with_another_template_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let state = replicated_state(tmp.path()).await;
    let root = tmp.path().join("fleet");
    let primary_config = DatabaseFleetConfig {
        path: Some("{tenant}/primary.db".to_owned()),
        ..config(&root)
    };
    let primary = fleet(&primary_config, Some(HOUR));
    assert_eq!(
        install_replication(&state, &primary, &primary_config).unwrap(),
        ReplicationInstall::Installed
    );
    let analytics_config = DatabaseFleetConfig {
        path: Some("{tenant}/analytics.db".to_owned()),
        ..config(&root)
    };
    let analytics = fleet(&analytics_config, Some(HOUR));

    let error = install_replication(&state, &analytics, &analytics_config).unwrap_err();

    assert!(
        matches!(error, InstallReplicationError::RootInUse { .. }),
        "{error}"
    );
    assert!(analytics.replication().is_none(), "nothing installed");
    stop(&primary);
}

#[tokio::test]
async fn with_replication_off_nothing_is_installed() {
    let tmp = tempfile::tempdir().unwrap();
    let config = config(&tmp.path().join("fleet"));
    let fleet = fleet(&config, Some(HOUR));

    let outcome = install_replication(&AppState::for_test(), &fleet, &config).unwrap();

    assert_eq!(outcome, ReplicationInstall::NoReplication);
    assert!(fleet.replication().is_none());
}

#[tokio::test]
async fn ownership_and_no_replica_logs_one_warning_at_boot() {
    let tmp = tempfile::tempdir().unwrap();
    let config = owned(&tmp.path().join("fleet"));
    let fleet = fleet(&config, Some(HOUR));
    let state = AppState::for_test();

    let warnings = Warnings::capture();
    for _ in 0..2 {
        let _ = install_replication(&state, &fleet, &config).unwrap();
    }

    assert_eq!(warnings.matching(UNREPLICATED_WARNING), 1, "{warnings:?}");
}

#[tokio::test]
async fn no_ownership_or_a_rebuild_engine_logs_no_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let plain_config = config(&tmp.path().join("plain"));
    let plain = fleet(&plain_config, Some(HOUR));
    let rebuild_config = owned(&tmp.path().join("rebuild"));
    let rebuild = fleet(&rebuild_config, None);
    let state = AppState::for_test();

    let warnings = Warnings::capture();
    let _ = install_replication(&state, &plain, &plain_config).unwrap();
    let _ = install_replication(&state, &rebuild, &rebuild_config).unwrap();

    assert_eq!(warnings.matching(UNREPLICATED_WARNING), 0, "{warnings:?}");
}

#[tokio::test]
async fn an_installed_replica_logs_no_warning() {
    let tmp = tempfile::tempdir().unwrap();
    let state = replicated_state(tmp.path()).await;
    let config = owned(&tmp.path().join("fleet"));
    let fleet = fleet(&config, Some(HOUR));

    let warnings = Warnings::capture();
    let _ = install_replication(&state, &fleet, &config).unwrap();

    assert_eq!(warnings.matching(UNREPLICATED_WARNING), 0, "{warnings:?}");
    stop(&fleet);
}
