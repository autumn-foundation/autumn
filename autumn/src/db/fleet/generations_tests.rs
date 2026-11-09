//! Tests of generation shipping and restore, run on [`TextEngine`] in
//! `ShipMode::Generations`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    let destination: Arc<dyn ReplicaDestination> =
        Arc::new(FileDestination::new(dir.join("replicas")).unwrap());
    host_on(
        dir,
        volume,
        control_db,
        engine,
        restore_missing,
        retention,
        destination,
    )
}

/// As [`host_with`], with the destination `destination`.
fn host_on(
    dir: &Path,
    volume: &str,
    control_db: &str,
    engine: TextEngine,
    restore_missing: bool,
    retention: Duration,
    destination: Arc<dyn ReplicaDestination>,
) -> Host {
    let fleet = DatabaseFleet::with_engine(
        &config(&dir.join(volume), restore_missing, retention),
        engine,
    )
    .unwrap();
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

// ---- A close-time generation that did not ship ----

/// A file destination that refuses each put while `refuse` is set.
struct Refusing {
    inner: FileDestination,
    refuse: AtomicBool,
    /// When set, the next put tells `entered` and waits on `leave`.
    gate: std::sync::Mutex<Option<Gate>>,
}

struct Gate {
    entered: std::sync::mpsc::Sender<()>,
    leave: std::sync::mpsc::Receiver<()>,
}

impl Refusing {
    fn set(&self, refuse: bool) {
        self.refuse.store(refuse, Ordering::SeqCst);
    }

    /// Hold the next put until the returned sender sends. The receiver
    /// gets a message when the put starts.
    fn hold_next_put(&self) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (entered, started) = std::sync::mpsc::channel();
        let (go, leave) = std::sync::mpsc::channel();
        *self.gate.lock().unwrap() = Some(Gate { entered, leave });
        (started, go)
    }

    fn check(&self) -> Result<(), crate::replication::DestinationError> {
        let gate = self.gate.lock().unwrap().take();
        if let Some(gate) = gate {
            gate.entered.send(()).unwrap();
            gate.leave.recv_timeout(Duration::from_secs(30)).unwrap();
        }
        if self.refuse.load(Ordering::SeqCst) {
            Err(crate::replication::DestinationError::Io {
                op: "put",
                detail: "the destination refuses puts".to_owned(),
            })
        } else {
            Ok(())
        }
    }
}

impl ReplicaDestination for Refusing {
    fn describe(&self) -> String {
        "refusing".to_owned()
    }
    fn put(&self, key: &str, body: &[u8]) -> Result<(), crate::replication::DestinationError> {
        self.check()?;
        self.inner.put(key, body)
    }
    fn put_file(&self, key: &str, path: &Path) -> Result<(), crate::replication::DestinationError> {
        self.check()?;
        self.inner.put_file(key, path)
    }
    fn get(&self, key: &str) -> Result<Vec<u8>, crate::replication::DestinationError> {
        self.inner.get(key)
    }
    fn get_to_file(
        &self,
        key: &str,
        path: &Path,
    ) -> Result<(), crate::replication::DestinationError> {
        self.inner.get_to_file(key, path)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, crate::replication::DestinationError> {
        self.inner.list(prefix)
    }
    fn delete(&self, key: &str) -> Result<(), crate::replication::DestinationError> {
        self.inner.delete(key)
    }
}

/// A host on a destination that refuses puts on demand. It shares
/// `dir/replicas` with [`host`].
fn refusing_host(dir: &Path, volume: &str) -> (Host, Arc<Refusing>) {
    let refusing = Arc::new(Refusing {
        inner: FileDestination::new(dir.join("replicas")).unwrap(),
        refuse: AtomicBool::new(false),
        gate: std::sync::Mutex::new(None),
    });
    let host = host_on(
        dir,
        volume,
        "control.db",
        generations_engine(HOUR),
        false,
        Duration::ZERO,
        Arc::clone(&refusing) as Arc<dyn ReplicaDestination>,
    );
    (host, refusing)
}

/// Write `value`, then close `key` while the destination refuses puts.
async fn close_unshipped(host: &Host, refusing: &Refusing, key: &FleetDbKey, value: &str) {
    write(host, key, value).await;
    refusing.set(true);
    host.fleet.close(key).await;
    refusing.set(false);
}

/// One loop pass, on a blocking thread.
async fn tick(host: &Host) {
    let replication = Arc::clone(&host.replication);
    crate::time::spawn_blocking(move || replication.tick_all())
        .await
        .unwrap();
}

/// The shutdown flush, on a blocking thread.
async fn flush(host: &Host, deadline: Duration) -> Vec<String> {
    let replication = Arc::clone(&host.replication);
    crate::time::spawn_blocking(move || replication.flush_closing(deadline))
        .await
        .unwrap()
}

async fn unshipped_detail(host: &Host) -> serde_json::Value {
    host.replication.indicator().check().await.details["unshipped"].clone()
}

#[tokio::test]
async fn a_failed_close_ship_is_kept_and_the_ranges_lapse() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let _ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    write(&a, &key, "a1").await;
    refusing.set(true);
    a.fleet.close_all().await;
    assert_eq!(
        a.replication.unshipped_generations(),
        1,
        "the close kept it"
    );
    assert!(complete(&a, &key).is_empty(), "nothing shipped");
    assert_eq!(unshipped_detail(&a).await, serde_json::json!([key.name()]));
    assert!(
        !a.fleet.release_ownership().await,
        "an unshipped generation makes the ranges lapse"
    );
}

#[tokio::test]
async fn a_retry_that_works_ships_the_generation_and_drops_the_record() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let _ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    write(&a, &key, "a1").await;
    refusing.set(true);
    a.fleet.close_all().await;
    tick(&a).await;
    assert_eq!(
        a.replication.unshipped_generations(),
        1,
        "a failed retry keeps it"
    );
    refusing.set(false);
    tick(&a).await;
    assert_eq!(
        a.replication.unshipped_generations(),
        0,
        "the retry shipped"
    );
    assert_eq!(complete(&a, &key).len(), 1);
    assert_eq!(unshipped_detail(&a).await, serde_json::json!([]));
    assert!(
        a.fleet.release_ownership().await,
        "nothing is left unshipped"
    );

    // A new volume restores the write.
    let b = host(tmp.path(), "vol-b", true);
    assert_eq!(read(&b, &key).await, ["a1"]);
    b.fleet.close_all().await;
}

#[tokio::test]
async fn the_final_flush_ships_an_unshipped_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let key = acme(&a.fleet);
    close_unshipped(&a, &refusing, &key, "a1").await;
    assert_eq!(a.replication.unshipped_generations(), 1);
    assert!(flush(&a, Duration::from_secs(5)).await.is_empty());
    assert_eq!(a.replication.unshipped_generations(), 0);
    assert_eq!(complete(&a, &key).len(), 1);
    a.fleet.close_all().await;
    let b = host(tmp.path(), "vol-b", true);
    assert_eq!(read(&b, &key).await, ["a1"]);
    b.fleet.close_all().await;
}

#[tokio::test]
async fn the_final_flush_names_a_generation_it_could_not_ship() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let key = acme(&a.fleet);
    close_unshipped(&a, &refusing, &key, "a1").await;
    refusing.set(true);
    let left = flush(&a, Duration::from_millis(300)).await;
    assert_eq!(left, [key.name()]);
    assert_eq!(a.replication.unshipped_generations(), 1, "the record stays");
    assert_eq!(unshipped_detail(&a).await, serde_json::json!([key.name()]));
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_delete_drops_the_record_and_ships_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let key = acme(&a.fleet);
    close_unshipped(&a, &refusing, &key, "gone").await;
    assert_eq!(a.replication.unshipped_generations(), 1);
    assert!(a.fleet.delete(&key).await.unwrap());
    assert_eq!(
        a.replication.unshipped_generations(),
        0,
        "the delete dropped it"
    );
    tick(&a).await;
    assert!(complete(&a, &key).is_empty(), "nothing ships for it");
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_restore_drops_the_record_and_ships_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let key = acme(&a.fleet);
    write(&a, &key, "a1").await;
    a.fleet.ship_now(&key).await.unwrap();
    close_unshipped(&a, &refusing, &key, "a2").await;
    assert_eq!(a.replication.unshipped_generations(), 1);
    a.fleet.restore_latest(&key).await.unwrap();
    assert_eq!(
        a.replication.unshipped_generations(),
        0,
        "the restore dropped it"
    );
    tick(&a).await;
    assert_eq!(complete(&a, &key).len(), 1, "nothing ships for it");
    assert_eq!(read(&a, &key).await, ["a1"]);
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_fence_drops_the_record_and_ships_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let _ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    close_unshipped(&a, &refusing, &key, "a1").await;
    assert_eq!(a.replication.unshipped_generations(), 1);
    // Another host took the slot and wrote its epoch marker.
    let root = a.replication.root_for(&key);
    a.replication
        .destination()
        .put(&segment::epoch_marker_key(&root, 9), b"{}")
        .unwrap();
    tick(&a).await;
    assert_eq!(
        a.replication.unshipped_generations(),
        0,
        "the fence dropped it"
    );
    assert!(complete(&a, &key).is_empty(), "nothing ships for it");
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_fenced_close_keeps_no_record() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let _ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    write(&a, &key, "a1").await;
    let root = a.replication.root_for(&key);
    a.replication
        .destination()
        .put(&segment::epoch_marker_key(&root, 9), b"{}")
        .unwrap();
    a.fleet.close(&key).await;
    assert_eq!(a.replication.unshipped_generations(), 0);
    assert!(complete(&a, &key).is_empty(), "the fence refused the ship");
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_close_past_the_shutdown_cutoff_keeps_the_generation() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    write(&a, &key, "a1").await;
    #[allow(clippy::disallowed_methods, reason = "a cutoff in the past")]
    let past = std::time::Instant::now();
    a.replication.set_shutdown_cutoff(Some(past));
    a.fleet.close(&key).await;
    assert_eq!(
        a.replication.unshipped_generations(),
        1,
        "skipped, then kept"
    );
    tick(&a).await;
    assert_eq!(
        a.replication.unshipped_generations(),
        1,
        "no retry past the cutoff"
    );
    a.replication.set_shutdown_cutoff(None);
    tick(&a).await;
    assert_eq!(a.replication.unshipped_generations(), 0);
    assert_eq!(complete(&a, &key).len(), 1);
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_retry_waits_while_the_database_is_open_and_its_ship_drops_the_record() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let key = acme(&a.fleet);
    close_unshipped(&a, &refusing, &key, "a1").await;
    write(&a, &key, "a2").await;
    tick(&a).await;
    assert_eq!(
        a.replication.unshipped_generations(),
        1,
        "an open database is not retried"
    );
    assert!(complete(&a, &key).is_empty());
    a.fleet.ship_now(&key).await.unwrap();
    assert_eq!(
        a.replication.unshipped_generations(),
        0,
        "the ship of the open database holds the closed writes"
    );
    a.fleet.close_all().await;
    let b = host(tmp.path(), "vol-b", true);
    assert_eq!(read(&b, &key).await, ["a1", "a2"]);
    b.fleet.close_all().await;
}

#[tokio::test]
async fn a_lapsed_lease_keeps_the_record_and_ships_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    close_unshipped(&a, &refusing, &key, "a1").await;
    // No newer epoch is known: the host only lost its lease.
    ownership.release_all().await;
    assert_eq!(ownership.epoch_for(key.routing_slot()), None);
    tick(&a).await;
    assert_eq!(
        a.replication.unshipped_generations(),
        1,
        "a lapsed lease is not a fence"
    );
    assert!(
        complete(&a, &key).is_empty(),
        "the host does not hold the slot"
    );
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_second_failed_close_replaces_the_record_and_its_retry_ships_the_newest_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let key = acme(&a.fleet);
    close_unshipped(&a, &refusing, &key, "a1").await;
    close_unshipped(&a, &refusing, &key, "a2").await;
    assert_eq!(
        a.replication.unshipped_generations(),
        1,
        "one record per key"
    );
    tick(&a).await;
    assert_eq!(a.replication.unshipped_generations(), 0);
    a.fleet.close_all().await;
    let b = host(tmp.path(), "vol-b", true);
    assert_eq!(read(&b, &key).await, ["a1", "a2"]);
    b.fleet.close_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_does_not_wait_for_the_upload_of_a_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let key = acme(&a.fleet);
    close_unshipped(&a, &refusing, &key, "a1").await;
    let (started, go) = refusing.hold_next_put();
    let retry = {
        let replication = Arc::clone(&a.replication);
        crate::time::spawn_blocking(move || replication.tick_all())
    };
    // The retry took its snapshot, and its first put waits now.
    crate::time::spawn_blocking(move || started.recv_timeout(Duration::from_secs(30)))
        .await
        .unwrap()
        .unwrap();
    let opened = tokio::time::timeout(Duration::from_secs(10), a.fleet.open(&key)).await;
    assert!(
        matches!(opened, Ok(Ok(_))),
        "the hold ended after the snapshot"
    );
    drop(opened);
    go.send(()).unwrap();
    retry.await.unwrap();
    assert_eq!(a.replication.unshipped_generations(), 0);
    assert_eq!(complete(&a, &key).len(), 1);
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_lapse_while_open_keeps_the_writes_as_unshipped_work() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    write(&a, &key, "a1").await;
    // The lease lapses: no newer epoch is known.
    ownership.release_all().await;
    assert_eq!(a.fleet.fence_unowned(), 1);
    a.fleet.close(&key).await;
    assert_eq!(
        a.replication.unshipped_generations(),
        1,
        "a lapse is not a fence"
    );
    assert!(complete(&a, &key).is_empty(), "the close shipped nothing");
    assert!(!a.fleet.release_ownership().await, "the ranges lapse");
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_failed_restore_puts_the_record_back() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let key = acme(&a.fleet);
    close_unshipped(&a, &refusing, &key, "a1").await;
    // No generation is there: the restore fails, and the file stays.
    let restored = a.fleet.restore_latest(&key).await;
    assert!(
        matches!(restored, Err(FleetError::NotFound { .. })),
        "{restored:?}"
    );
    assert_eq!(
        a.replication.unshipped_generations(),
        1,
        "the record is back"
    );
    tick(&a).await;
    assert_eq!(a.replication.unshipped_generations(), 0);
    assert_eq!(complete(&a, &key).len(), 1, "it shipped");
    a.fleet.close_all().await;
}

#[tokio::test]
async fn a_record_whose_files_are_gone_ships_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (a, refusing) = refusing_host(tmp.path(), "vol-a");
    let key = acme(&a.fleet);
    close_unshipped(&a, &refusing, &key, "a1").await;
    // Another process removed the file.
    std::fs::remove_file(a.fleet.path_of(&key)).unwrap();
    tick(&a).await;
    assert_eq!(a.replication.unshipped_generations(), 0);
    assert!(complete(&a, &key).is_empty());
    a.fleet.close_all().await;
}

/// The system clock after a commit, and a wait until it moves past that
/// reading. A coarse clock then cannot give the next ship the same value.
fn wall_after_commit() -> chrono::DateTime<chrono::Utc> {
    let at = crate::time::ClockSource::now(&crate::time::SystemClock);
    while crate::time::ClockSource::now(&crate::time::SystemClock) <= at {
        std::thread::yield_now();
    }
    at
}

#[tokio::test]
async fn a_shipped_generation_sets_replicated_through() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    let committed_at = wall_after_commit();
    assert_eq!(
        a.fleet.replicated_through(&key),
        None,
        "nothing shipped yet"
    );
    a.fleet.ship_now(&key).await.unwrap();
    let through = a.fleet.replicated_through(&key).expect("shipped");
    assert!(through >= committed_at, "{through} < {committed_at}");
    a.fleet
        .wait_replicated(&key, committed_at, Duration::ZERO)
        .await
        .expect("covered");

    // An unchanged database ships nothing new, and is still covered.
    a.fleet.ship_now(&key).await.unwrap();
    assert!(a.fleet.replicated_through(&key) >= Some(through));

    // The close ships the last generation. The bound stays for waiters.
    write(&a, &key, "last").await;
    let last_at = wall_after_commit();
    a.fleet.close_all().await;
    assert!(
        a.fleet
            .replicated_through(&key)
            .is_some_and(|through| through >= last_at),
        "the close-time ship covers the last write"
    );
}

#[tokio::test]
async fn a_fenced_generation_ship_drops_the_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let a = host(tmp.path(), "vol-a", false);
    let store = Arc::new(MemoryOwnership::new(Arc::new(crate::time::SystemClock)));
    let _ownership = own(&a, &store, "a").await;
    let key = acme(&a.fleet);
    write(&a, &key, "plan").await;
    let committed_at = wall_after_commit();
    a.fleet.ship_now(&key).await.unwrap();
    assert!(a.fleet.replicated_through(&key) >= Some(committed_at));

    // Another host took the slot and wrote its epoch marker.
    let root = a.replication.root_for(&key);
    a.replication
        .destination()
        .put(&segment::epoch_marker_key(&root, 9), b"{}")
        .unwrap();
    write(&a, &key, "late").await;
    assert!(matches!(
        a.fleet.ship_now(&key).await,
        Err(FleetError::Fenced { .. })
    ));
    assert_eq!(a.fleet.replicated_through(&key), None);
    let waited = a
        .fleet
        .wait_replicated(&key, committed_at, Duration::from_secs(30))
        .await;
    assert!(
        matches!(
            waited,
            Err(crate::db::fleet_replication::ReplicatedWaitError::Fenced { .. })
        ),
        "{waited:?}"
    );
    a.fleet.close_all().await;
}
