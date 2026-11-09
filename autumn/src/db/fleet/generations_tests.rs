//! Tests of generation shipping and restore, run on [`TextEngine`] in
//! `ShipMode::Generations`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::test_engine::TextEngine;
use super::{DatabaseFleet, FleetError};
use crate::config::DatabaseFleetConfig;
use crate::db::fleet_ownership::{FleetOwnership, MemoryOwnership, OwnerId, SlotRange};
use crate::db::fleet_replication::FleetReplication;
use crate::db::fleet_tombstones::FleetTombstones;
use crate::fleet_layout::{FleetDbKey, FleetMode};
use crate::replication::{FileDestination, ReplicaDestination, ReplicationSettings, segment};

const BASE: &str = "app/gen";
const HOUR: Duration = Duration::from_hours(1);
const TTL: Duration = Duration::from_secs(30);

fn config(root: &Path, restore_missing: bool, retention: Duration) -> DatabaseFleetConfig {
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
        restore_missing,
        ownership: None,
        replica_retention_after_delete: retention,
        restore_check_sample: 0,
        replication_workers: 1,
        mmap_size: 0,
        cache_size_kib: 0,
    }
}

fn control(
    dir: &Path,
    name: &str,
) -> diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection> {
    crate::db::create_pool(&crate::config::DatabaseConfig {
        url: Some(format!("sqlite://{}", dir.join(name).display())),
        ..Default::default()
    })
    .unwrap()
    .unwrap()
}

struct Host {
    fleet: DatabaseFleet<TextEngine>,
    replication: Arc<FleetReplication>,
}

/// A host with volume `volume` and control database `control_db`. Every
/// host shares the replica directory `dir/replicas`.
fn host_with(
    dir: &Path,
    volume: &str,
    control_db: &str,
    engine: TextEngine,
    restore_missing: bool,
    retention: Duration,
) -> Host {
    let fleet = DatabaseFleet::with_engine(
        &config(&dir.join(volume), restore_missing, retention),
        engine,
    )
    .unwrap();
    let destination: Arc<dyn ReplicaDestination> =
        Arc::new(FileDestination::new(dir.join("replicas")).unwrap());
    let clock: Arc<dyn crate::time::ClockSource> = Arc::new(crate::time::SystemClock);
    let tombstones = Arc::new(FleetTombstones::new(
        control(dir, control_db),
        Arc::clone(&clock),
    ));
    let replication = Arc::new(
        FleetReplication::new(
            destination,
            ReplicationSettings {
                database_path: std::path::PathBuf::new(),
                root: BASE.to_owned(),
                sync_interval: Duration::from_secs(1),
                snapshot_interval: HOUR,
                max_wal_bytes: 4 * 1024 * 1024,
                retention: HOUR,
                verify_interval: None,
            },
            Duration::from_secs(30),
            restore_missing,
            clock,
        )
        .with_tombstones(tombstones)
        .with_retention_after_delete(retention),
    );
    assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
    Host { fleet, replication }
}

const fn generations_engine(every: Duration) -> TextEngine {
    let mut engine = TextEngine::new(100);
    engine.every = Some(every);
    engine
}

fn host(dir: &Path, volume: &str, restore_missing: bool) -> Host {
    host_with(
        dir,
        volume,
        "control.db",
        generations_engine(HOUR),
        restore_missing,
        Duration::ZERO,
    )
}

fn acme<E: super::FleetEngine>(fleet: &DatabaseFleet<E>) -> FleetDbKey {
    fleet.key_for("acme").unwrap()
}

/// Every object of the replica of `key`.
fn objects(host: &Host, key: &FleetDbKey) -> Vec<String> {
    let root = host.replication.root_for(key);
    host.replication
        .destination()
        .list(&format!("{root}/"))
        .unwrap()
}

/// The ids of the complete generations of `key`, oldest first.
fn complete(host: &Host, key: &FleetDbKey) -> Vec<String> {
    let root = host.replication.root_for(key);
    super::generations::complete_generations(&root, &objects(host, key))
        .into_iter()
        .map(|g| g.id)
        .collect()
}

async fn write(host: &Host, key: &FleetDbKey, value: &str) {
    let db = host.fleet.open(key).await.unwrap();
    TextEngine::write(db.handle(), value).unwrap();
}

async fn read(host: &Host, key: &FleetDbKey) -> Vec<String> {
    let db = host.fleet.open(key).await.unwrap();
    TextEngine::read(db.handle()).unwrap()
}

fn owner(name: &str) -> OwnerId {
    OwnerId::new(name).unwrap()
}

async fn own(host: &Host, store: &Arc<MemoryOwnership>, name: &str) -> Arc<FleetOwnership> {
    let ownership = Arc::new(
        FleetOwnership::new(
            Arc::clone(store) as _,
            owner(name),
            vec![SlotRange::ALL],
            TTL,
        )
        .unwrap(),
    );
    host.fleet
        .start_ownership(Arc::clone(&ownership))
        .await
        .unwrap();
    ownership
}

#[tokio::test]
async fn ship_now_writes_the_files_then_the_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    let report = a.fleet.ship_now(&key).await.unwrap();
    assert_eq!(report.shipped, 1);
    assert_eq!(report.reused, 0);
    let id = report.generation.expect("a generation");
    assert_eq!(complete(&a, &key), std::slice::from_ref(&id));
    let root = a.replication.root_for(&key);
    let manifest: super::generations::Manifest = serde_json::from_slice(
        &a.replication
            .destination()
            .get(&super::generations::manifest_key(&root, &id))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest.files.len(), 1);
    let file = a.fleet.path_of(&key);
    assert_eq!(
        manifest.files[0].path,
        file.file_name().unwrap().to_string_lossy()
    );
    a.fleet.close_all().await;
}

#[tokio::test]
async fn an_unchanged_database_ships_nothing_and_a_change_ships_again() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    a.fleet.ship_now(&key).await.unwrap();
    let before = objects(&a, &key);
    let again = a.fleet.ship_now(&key).await.unwrap();
    assert_eq!(again.generation, None);
    assert_eq!(again.shipped, 0);
    assert_eq!(objects(&a, &key), before, "no object is written");
    write(&a, &key, "ship").await;
    let changed = a.fleet.ship_now(&key).await.unwrap();
    assert!(changed.generation.is_some());
    assert_eq!(complete(&a, &key).len(), 2);
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_new_volume_restores_the_newest_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    a.fleet.ship_now(&key).await.unwrap();
    write(&a, &key, "ship").await;
    a.fleet.ship_now(&key).await.unwrap();
    a.fleet.close_all().await;

    let b = host(tmp.path(), "vol-b", true);
    assert_eq!(read(&b, &key).await, ["plan", "ship"]);
    b.fleet.close_all().await;
}

#[tokio::test]
async fn close_ships_the_last_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    write(&a, &key, "unshipped").await;
    assert!(complete(&a, &key).is_empty());
    a.fleet.close(&key).await;
    assert_eq!(complete(&a, &key).len(), 1, "the close shipped");
    a.fleet.close_all().await;
}

#[tokio::test]
async fn the_worker_ships_every_interval() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host_with(
        tmp.path(),
        "vol-a",
        "control.db",
        generations_engine(Duration::from_millis(50)),
        false,
        Duration::ZERO,
    );
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    for _ in 0..200 {
        if !complete(&a, &key).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!complete(&a, &key).is_empty(), "the interval shipped");
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_fenced_ship_is_refused_and_writes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    a.fleet.ship_now(&key).await.unwrap();
    // Another host took the slot and wrote its epoch marker.
    let root = a.replication.root_for(&key);
    a.replication
        .destination()
        .put(&segment::epoch_marker_key(&root, 9), b"{}")
        .unwrap();
    write(&a, &key, "late").await;
    let before = objects(&a, &key);
    let refused = a.fleet.ship_now(&key).await;
    assert!(
        matches!(
            refused,
            Err(FleetError::Fenced {
                epoch: 1,
                newest: 9,
                ..
            })
        ),
        "{refused:?}"
    );
    assert_eq!(objects(&a, &key), before, "a fenced ship writes nothing");
    assert_eq!(
        ownership.epoch_for(key.routing_slot()),
        None,
        "the range is lost"
    );
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_delete_purges_the_generations_and_a_restore_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    write(&a, &key, "gone").await;
    a.fleet.ship_now(&key).await.unwrap();
    a.fleet.close(&key).await;
    assert!(a.fleet.delete(&key).await.unwrap());
    assert!(
        complete(&a, &key).is_empty(),
        "the purge removed the generations"
    );
    let left = objects(&a, &key);
    assert!(
        left.iter().all(|object| !object.contains("/generations/")),
        "the purge removed every file and marker of a generation: {left:?}"
    );
    let restored = a.fleet.restore_latest(&key).await;
    assert!(
        matches!(restored, Err(FleetError::NotFound { .. })),
        "{restored:?}"
    );
    assert!(
        read(&a, &key).await.is_empty(),
        "a new database holds no old data"
    );
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_delete_in_its_retention_blocks_restore_missing_and_an_explicit_restore_undoes_it() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host_with(
        tmp.path(),
        "vol-a",
        "control-a.db",
        generations_engine(HOUR),
        false,
        HOUR,
    );
    let key = acme(&a.fleet);
    write(&a, &key, "kept").await;
    a.fleet.ship_now(&key).await.unwrap();
    a.fleet.close(&key).await;
    assert!(a.fleet.delete(&key).await.unwrap());
    assert_eq!(
        complete(&a, &key).len(),
        1,
        "the retention keeps the replica"
    );

    // Another host, with its own control database: the marker blocks it.
    let b = host_with(
        tmp.path(),
        "vol-b",
        "control-b.db",
        generations_engine(HOUR),
        true,
        HOUR,
    );
    let opened = b.fleet.open(&key).await;
    assert!(
        matches!(opened, Err(FleetError::Deleting { .. })),
        "{opened:?}"
    );

    // The host that deleted it undoes the delete.
    a.fleet.restore_latest(&key).await.unwrap();
    assert_eq!(read(&a, &key).await, ["kept"]);
    a.fleet.close_all().await;
    b.fleet.close_all().await;
}

#[tokio::test]
async fn a_prune_keeps_two_generations() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    for value in ["one", "two", "three", "four"] {
        write(&a, &key, value).await;
        a.fleet.ship_now(&key).await.unwrap();
    }
    assert_eq!(complete(&a, &key).len(), 2);
    a.fleet.close_all().await;
    let b = host(tmp.path(), "vol-b", true);
    assert_eq!(read(&b, &key).await, ["one", "two", "three", "four"]);
    b.fleet.close_all().await;
}

#[tokio::test]
async fn a_host_that_gets_its_range_back_serves_the_newer_owners_data() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let a = host(tmp.path(), "vol-a", true);
    let own_a = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    write(&a, &key, "a1").await;
    // A close, not `close_all`: A serves again later.
    a.fleet.close(&key).await;
    assert!(a.fleet.release_ownership().await);

    let b = host(tmp.path(), "vol-b", true);
    own(&b, &store, "b").await;
    write(&b, &key, "b1").await;
    b.fleet.close_all().await;
    assert!(b.fleet.release_ownership().await);

    // A claims again. Its file holds only `a1`: the replica is newer.
    assert_eq!(own_a.refresh().await.claimed.len(), 1);
    assert_eq!(read(&a, &key).await, ["a1", "b1"]);
    a.fleet.close_all().await;
}

#[tokio::test]
async fn stats_and_health_report_the_ship_mode_and_the_rpo() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let stats = a.fleet.stats();
    assert_eq!(stats.ship_mode, "generations");
    assert_eq!(stats.rpo_secs, Some(HOUR.as_secs()));
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    a.fleet.ship_now(&key).await.unwrap();
    let health = a.replication.indicator().check().await;
    assert_eq!(health.status, crate::actuator::HealthStatus::Up);
    let details = health.details;
    assert_eq!(details["ship_mode"], "generations");
    assert_eq!(details["rpo_seconds"], HOUR.as_secs());
    assert_eq!(details["generations"]["databases"], 1);
    a.fleet.close_all().await;

    let plain = DatabaseFleet::with_engine(
        &config(&tmp.path().join("vol-c"), false, Duration::ZERO),
        TextEngine::new(1),
    )
    .unwrap();
    let stats = plain.stats();
    assert_eq!(stats.ship_mode, "rebuild");
    assert_eq!(stats.rpo_secs, None);
}

#[tokio::test]
async fn restore_latest_rebuilds_a_rebuild_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = DatabaseFleet::with_engine(
        &config(&tmp.path().join("vol"), false, Duration::ZERO),
        TextEngine::new(1),
    )
    .unwrap();
    let key = acme(&fleet);
    let db = fleet.open(&key).await.unwrap();
    TextEngine::write(db.handle(), "derived").unwrap();
    drop(db);
    fleet.restore_latest(&key).await.unwrap();
    let db = fleet.open(&key).await.unwrap();
    assert_eq!(TextEngine::read(db.handle()).unwrap(), ["rebuilt"]);
    drop(db);
    assert!(matches!(
        fleet.ship_now(&key).await,
        Err(FleetError::NotReplicated { .. })
    ));
    // A rebuild never brings back a deleted database.
    fleet.close(&key).await;
    assert!(fleet.delete(&key).await.unwrap());
    let refused = fleet.restore_latest(&key).await;
    assert!(
        matches!(refused, Err(FleetError::NotFound { .. })),
        "{refused:?}"
    );
    assert!(!fleet.exists(&key));
    fleet.close_all().await;
}

#[tokio::test]
async fn a_sidecar_scope_is_fenced_by_a_newer_epoch_at_the_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let _ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    let db = a.fleet.open(&key).await.unwrap();
    let scope = db.sidecar_replica("duckdb").unwrap().unwrap();
    let root = a.replication.root_for(&key);
    let object = format!("{root}/duckdb/generations/x/a");
    scope.put(&object, b"1").unwrap();
    a.replication
        .destination()
        .put(&segment::epoch_marker_key(&root, 9), b"{}")
        .unwrap();
    assert!(matches!(
        scope.put(&object, b"2"),
        Err(crate::replication::DestinationError::Fenced { .. })
    ));
    drop(db);
    a.fleet.close_all().await;
}

#[test]
fn a_zero_interval_ships_every_second() {
    use super::ShipMode;
    let mode = ShipMode::Generations {
        every: Duration::ZERO,
    };
    assert_eq!(mode.every(), Some(Duration::from_secs(1)));
    assert_eq!(ShipMode::Rebuild.every(), None);
    assert_eq!(mode.name(), "generations");
}

#[tokio::test]
async fn a_failed_ship_shows_in_health() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let _ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    let root = a.replication.root_for(&key);
    a.replication
        .destination()
        .put(&segment::epoch_marker_key(&root, 9), b"{}")
        .unwrap();
    assert!(a.fleet.ship_now(&key).await.is_err());
    let details = a.replication.indicator().check().await.details;
    assert_eq!(details["generations"]["failing"], 1, "{details:?}");
    assert!(details["generations"]["last_error"].is_string());
    a.fleet.close_all().await;
}

#[tokio::test]
async fn the_restore_check_restores_a_generation_and_finds_a_bad_file() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    a.fleet.ship_now(&key).await.unwrap();
    let report = a.fleet.check_replica_sample(1).await.unwrap();
    assert_eq!(report.passed, [key.name()]);
    // Damage the one file of the generation.
    let file = objects(&a, &key)
        .into_iter()
        .find(|object| object.contains("/files/"))
        .unwrap();
    a.replication.destination().put(&file, b"damaged").unwrap();
    let report = a.fleet.check_replica_sample(1).await.unwrap();
    assert_eq!(report.failed.len(), 1, "{report:?}");
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_tombstone_row_on_a_shared_control_database_blocks_another_host() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host_with(
        tmp.path(),
        "vol-a",
        "control.db",
        generations_engine(HOUR),
        false,
        HOUR,
    );
    let key = acme(&a.fleet);
    write(&a, &key, "kept").await;
    a.fleet.ship_now(&key).await.unwrap();
    a.fleet.close(&key).await;
    assert!(a.fleet.delete(&key).await.unwrap());
    // B shares the control database, so it reads the row itself.
    let b = host_with(
        tmp.path(),
        "vol-b",
        "control.db",
        generations_engine(HOUR),
        true,
        HOUR,
    );
    let opened = b.fleet.open(&key).await;
    assert!(
        matches!(opened, Err(FleetError::Deleting { .. })),
        "{opened:?}"
    );
    assert!(!b.fleet.exists(&key), "the row blocks the restore");
    a.fleet.close_all().await;
    b.fleet.close_all().await;
}

#[tokio::test]
async fn a_delete_ships_nothing_and_drops_the_slot() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    write(&a, &key, "gone").await;
    assert!(a.replication.current_generation_slot(&key).is_some());
    assert!(a.fleet.delete(&key).await.unwrap());
    assert!(
        a.replication.current_generation_slot(&key).is_none(),
        "the close dropped the slot"
    );
    assert!(complete(&a, &key).is_empty(), "a delete ships nothing");
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_new_owner_writes_its_epoch_marker_at_open() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let _ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    let db = a.fleet.open(&key).await.unwrap();
    let root = a.replication.root_for(&key);
    let marker = segment::epoch_marker_key(&root, 1);
    for _ in 0..200 {
        if objects(&a, &key).contains(&marker) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        objects(&a, &key).contains(&marker),
        "no ship yet, but a marker"
    );
    assert!(complete(&a, &key).is_empty());
    drop(db);
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_fail_back_keeps_a_file_newer_than_every_generation_above_its_stamp() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let _ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    write(&a, &key, "a1").await;
    a.fleet.ship_now(&key).await.unwrap();
    // A write after the last generation, of epoch 1.
    write(&a, &key, "a2").await;
    let path = a.fleet.path_of(&key);
    // The only generation is of epoch 1, the stamp of the file: it is
    // older than the file, so the file stays.
    let replaced = a
        .fleet
        .restore_generation_in_place(&a.replication, &key, None, &path, Some(1))
        .unwrap();
    assert!(!replaced);
    assert_eq!(read(&a, &key).await, ["a1", "a2"]);
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_passed_shutdown_cutoff_stops_a_ship() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    #[allow(clippy::disallowed_methods, reason = "a cutoff in the past")]
    let past = std::time::Instant::now();
    a.replication.set_shutdown_cutoff(Some(past));
    let refused = a.fleet.ship_now(&key).await;
    assert!(refused.is_err(), "{refused:?}");
    assert!(complete(&a, &key).is_empty());
    a.replication.set_shutdown_cutoff(None);
    a.fleet.close_all().await;
}

#[tokio::test]
async fn two_processes_on_one_volume_ship_restorable_generations() {
    let tmp = tempfile::tempdir().unwrap();
    // Two processes on one volume, each with its own fleet.
    let a = host(tmp.path(), "vol", false);
    let b = host(tmp.path(), "vol", false);
    let key = acme(&a.fleet);
    write(&a, &key, "a1").await;
    a.fleet.ship_now(&key).await.unwrap();
    // B ships three generations: the prune removes A's first one.
    for value in ["b1", "b2", "b3"] {
        write(&b, &key, value).await;
        b.fleet.ship_now(&key).await.unwrap();
    }
    // A ships again. It reads the newest manifest, not the one it shipped.
    write(&a, &key, "a2").await;
    a.fleet.ship_now(&key).await.unwrap();
    let report = a.fleet.check_replica_sample(1).await.unwrap();
    assert_eq!(report.passed, [key.name()], "{report:?}");
    a.fleet.close_all().await;
    b.fleet.close_all().await;
    let c = host(tmp.path(), "vol-c", true);
    assert_eq!(read(&c, &key).await, ["a1", "b1", "b2", "b3", "a2"]);
    c.fleet.close_all().await;
}
