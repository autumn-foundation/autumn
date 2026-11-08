//! Tests of the engine-agnostic fleet core, run on [`TextEngine`]. They
//! compile without `sqlite`.

use std::path::Path;
use std::time::Duration;

use super::test_engine::{TextEngine, TextHandle};
use super::{DatabaseFleet, FleetError};
use crate::config::DatabaseFleetConfig;
use crate::fleet_layout::{FleetDbKey, FleetMode};

fn config(root: &Path, max_open: usize, max_resident_bytes: u64) -> DatabaseFleetConfig {
    DatabaseFleetConfig {
        mode: FleetMode::Tenant,
        root: root.display().to_string(),
        path: None,
        max_open,
        max_resident_bytes,
        min_residency: Duration::ZERO,
        pool_size: 1,
        create_on_demand: Some(true),
        idle_close_secs: 0,
        restore_missing: false,
        ownership: None,
        replica_retention_after_delete: Duration::ZERO,
        restore_check_sample: 1,
        replication_workers: 1,
        mmap_size: 0,
        cache_size_kib: 0,
    }
}

fn fleet(root: &Path, max_open: usize, budget: u64) -> DatabaseFleet<TextEngine> {
    DatabaseFleet::with_engine(&config(root, max_open, budget), TextEngine::new(100)).unwrap()
}

fn key(fleet: &DatabaseFleet<TextEngine>, tenant: &str) -> FleetDbKey {
    fleet.key_for(tenant).unwrap()
}

/// Waits until `cond` holds, for at most 5 s.
async fn eventually(mut cond: impl FnMut() -> bool) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(cond(), "the condition did not hold in time");
}

#[tokio::test]
async fn open_creates_writes_and_reopens_after_close() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8, 0);
    let acme = key(&fleet, "acme");
    let db = fleet.open(&acme).await.unwrap();
    TextEngine::write(db.handle(), "plan").unwrap();
    assert!(fleet.exists(&acme));
    fleet.close(&acme).await;
    assert!(db.handle().is_closed(), "close calls the engine");
    assert!(db.closed().is_cancelled());
    let again = fleet.open(&acme).await.unwrap();
    assert_eq!(TextEngine::read(again.handle()).unwrap(), ["plan"]);
    assert_eq!(
        fleet
            .engine()
            .opens
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
    let stats = fleet.stats();
    assert_eq!(stats.open, 1);
    assert_eq!(stats.opens_total, 2);
    assert_eq!(stats.created_total, 1);
    assert_eq!(stats.closes_total, 1);
}

#[tokio::test]
async fn open_refuses_the_file_of_another_key() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8, 0);
    let (acme, globex) = (key(&fleet, "acme"), key(&fleet, "globex"));
    drop(fleet.open(&acme).await.unwrap());
    fleet.close(&acme).await;
    let target = fleet.path_of(&globex);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::copy(fleet.path_of(&acme), &target).unwrap();
    let error = fleet.open(&globex).await.unwrap_err();
    assert!(
        matches!(&error, FleetError::Misplaced { found, .. } if found == &acme.name()),
        "{error}"
    );
    assert_eq!(error.http_status(), http::StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(fleet.stats().open_failures_total, 1);
}

#[tokio::test]
async fn budget_closes_the_least_recently_used_idle_database() {
    let tmp = tempfile::tempdir().unwrap();
    // 100 bytes each: three databases are over a 250-byte budget.
    let fleet = fleet(tmp.path(), 64, 250);
    let keys: Vec<FleetDbKey> = ["a", "b", "c"].iter().map(|t| key(&fleet, t)).collect();
    for key in &keys {
        drop(fleet.open(key).await.unwrap());
    }
    eventually(|| fleet.stats().open == 2).await;
    let open = fleet.open_keys();
    assert!(!open.contains(&keys[0]), "the oldest goes: {open:?}");
    assert!(open.contains(&keys[1]) && open.contains(&keys[2]));
    let stats = fleet.stats();
    assert_eq!(stats.max_resident_bytes, 250);
    eventually(|| fleet.stats().resident_bytes == 200).await;
}

#[tokio::test]
async fn budget_never_closes_a_database_in_use() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 64, 250);
    let keys: Vec<FleetDbKey> = ["a", "b", "c"].iter().map(|t| key(&fleet, t)).collect();
    let first = fleet.open(&keys[0]).await.unwrap();
    let busy = first.handle().checkout();
    drop(first);
    for key in &keys[1..] {
        drop(fleet.open(key).await.unwrap());
    }
    eventually(|| fleet.stats().open == 2).await;
    let open = fleet.open_keys();
    assert!(open.contains(&keys[0]), "a busy database stays: {open:?}");
    assert!(
        !open.contains(&keys[1]),
        "the oldest idle one goes: {open:?}"
    );
    drop(busy);
}

#[tokio::test]
async fn budget_off_keeps_every_database_under_max_open() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 64, 0);
    for tenant in ["a", "b", "c", "d"] {
        drop(fleet.open(&key(&fleet, tenant)).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fleet.stats().open, 4);
    assert_eq!(fleet.stats().resident_bytes, 400);
}

#[tokio::test]
async fn background_open_finds_no_room_over_the_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 64, 100);
    let (a, b) = (key(&fleet, "a"), key(&fleet, "b"));
    // Make `b` on disk, then close it.
    drop(fleet.open(&b).await.unwrap());
    fleet.close(&b).await;
    let held = fleet.open(&a).await.unwrap();
    assert!(fleet.open_background(&b).await.unwrap().is_none());
    drop(held);
}

#[tokio::test]
async fn delete_removes_the_database() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = config(tmp.path(), 8, 0);
    cfg.create_on_demand = Some(false);
    let fleet = DatabaseFleet::with_engine(&cfg, TextEngine::new(100)).unwrap();
    let acme = key(&fleet, "acme");
    assert!(matches!(
        fleet.open(&acme).await,
        Err(FleetError::NotFound { .. })
    ));
    let create =
        DatabaseFleet::with_engine(&config(tmp.path(), 8, 0), TextEngine::new(100)).unwrap();
    drop(create.open(&acme).await.unwrap());
    create.close_all().await;
    let db = fleet.open(&acme).await.unwrap();
    assert!(fleet.delete(&acme).await.unwrap());
    assert!(db.closed().is_cancelled(), "delete closes the database");
    assert!(!fleet.exists(&acme));
    assert!(
        !fleet.delete(&acme).await.unwrap(),
        "nothing left to delete"
    );
    assert!(matches!(
        fleet.open(&acme).await,
        Err(FleetError::NotFound { .. })
    ));
    assert_eq!(fleet.stats().deleted_total, 1);
}

#[tokio::test]
async fn snapshot_writes_a_copy_that_opens_as_the_same_database() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(&tmp.path().join("a"), 8, 0);
    let acme = key(&fleet, "acme");
    let db = fleet.open(&acme).await.unwrap();
    TextEngine::write(db.handle(), "kept").unwrap();
    let dir = tmp.path().join("snap");
    std::fs::create_dir_all(&dir).unwrap();
    let files = fleet.snapshot(&acme, &dir).await.unwrap();
    assert_eq!(files.len(), 1);
    let other = fleet_at_copy(tmp.path(), &fleet, &acme, &files[0]);
    let copy = other.open(&acme).await.unwrap();
    assert_eq!(TextEngine::read(copy.handle()).unwrap(), ["kept"]);
}

fn fleet_at_copy(
    tmp: &Path,
    source: &DatabaseFleet<TextEngine>,
    key: &FleetDbKey,
    file: &Path,
) -> DatabaseFleet<TextEngine> {
    let other = fleet(&tmp.join("b"), 8, 0);
    let target = other.path_of(key);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::copy(file, &target).unwrap();
    assert_ne!(source.root(), other.root());
    other
}

#[tokio::test]
async fn generic_fleet_reports_its_ship_mode_and_handle() {
    use crate::db::fleet_engine::{FleetEngine as _, ShipMode};
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8, 0);
    assert!(matches!(fleet.engine().ship_mode(), ShipMode::Rebuild));
    let db = fleet.open(&key(&fleet, "acme")).await.unwrap();
    let handle: &TextHandle = db.handle();
    assert!(!handle.is_closed());
}

fn slot(n: u16) -> FleetDbKey {
    FleetDbKey::Slot(n)
}

#[test]
fn victims_are_taken_in_lru_order_for_the_count() {
    let idle = vec![(1, slot(1), 10), (2, slot(2), 10), (3, slot(3), 10)];
    assert_eq!(super::pick_victims(idle, 2, 0, 3), [slot(1), slot(2)]);
}

#[test]
fn victims_free_the_bytes_over_the_budget() {
    let idle = vec![(1, slot(1), 10), (2, slot(2), 30), (3, slot(3), 10)];
    // 35 bytes over: the first two free 40.
    assert_eq!(super::pick_victims(idle, 0, 35, 4), [slot(1), slot(2)]);
}

#[test]
fn the_count_and_the_budget_add_up() {
    let idle = vec![(1, slot(1), 5), (2, slot(2), 50), (3, slot(3), 5)];
    // One for the count; then 50 bytes still over.
    assert_eq!(super::pick_victims(idle, 1, 55, 4), [slot(1), slot(2)]);
}

#[test]
fn the_budget_skips_a_database_that_holds_no_memory() {
    let idle = vec![(1, slot(1), 0), (2, slot(2), 20)];
    assert_eq!(super::pick_victims(idle, 0, 10, 3), [slot(2)]);
}

#[test]
fn the_budget_keeps_the_last_open_database() {
    let idle = vec![(1, slot(1), 100)];
    assert!(super::pick_victims(idle, 0, 50, 1).is_empty());
    let idle = vec![(1, slot(1), 100), (2, slot(2), 100)];
    assert_eq!(super::pick_victims(idle, 0, 500, 2), [slot(1)]);
}

#[test]
fn nothing_over_means_no_victim() {
    let idle = vec![(1, slot(1), 100)];
    assert!(super::pick_victims(idle, 0, 0, 1).is_empty());
}

#[tokio::test]
async fn a_budget_below_one_database_keeps_it_open() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 64, 1);
    drop(fleet.open(&key(&fleet, "a")).await.unwrap());
    fleet.enforce_capacity(None);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fleet.stats().open, 1);
}

#[tokio::test]
async fn background_open_has_room_under_the_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 64, 1_000);
    let b = key(&fleet, "b");
    drop(fleet.open(&b).await.unwrap());
    fleet.close(&b).await;
    let held = fleet.open(&key(&fleet, "a")).await.unwrap();
    let opened = fleet.open_background(&b).await.unwrap();
    assert!(
        opened.is_some_and(|open| open.opened),
        "100 of 1000 bytes leaves room"
    );
    drop(held);
}

#[tokio::test]
async fn exists_and_list_follow_the_engine() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8, 0);
    let acme = key(&fleet, "acme");
    drop(fleet.open(&acme).await.unwrap());
    let stray = key(&fleet, "stray");
    std::fs::create_dir_all(fleet.path_of(&stray)).unwrap();
    // The text engine takes a directory as a database; a file engine would not.
    assert!(fleet.exists(&stray));
    assert_eq!(fleet.list().await.unwrap(), {
        let mut keys = vec![acme, stray];
        keys.sort();
        keys
    });
}

#[test]
fn sidecar_names_follow_the_rule() {
    use super::{RESERVED_SIDECAR_NAMES, sidecar_root};
    let root = "prod/fleet/tenant/acme";
    assert_eq!(
        sidecar_root(root, "duckdb").unwrap(),
        "prod/fleet/tenant/acme/duckdb"
    );
    assert!(sidecar_root(root, "a").is_ok());
    assert!(sidecar_root(root, "duck_db-2").is_ok());
    assert!(sidecar_root(root, &"a".repeat(32)).is_ok());
    for bad in [
        "",
        "DuckDB",
        "duck/db",
        "..",
        ".",
        "-duck",
        "_duck",
        "duck db",
        "dück",
        &"a".repeat(33),
    ] {
        assert!(sidecar_root(root, bad).is_err(), "{bad:?}");
    }
    for reserved in RESERVED_SIDECAR_NAMES {
        assert!(sidecar_root(root, reserved).is_err(), "{reserved}");
    }
}

#[tokio::test]
async fn a_fleet_with_no_replica_gives_no_sidecar_destination() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8, 0);
    let db = fleet.open(&key(&fleet, "acme")).await.unwrap();
    assert!(db.sidecar_replica("duckdb").unwrap().is_none());
    assert!(
        db.sidecar_replica("generations").is_err(),
        "the name is checked first"
    );
}

/// A delete retires the database before it waits for its users, so a
/// sidecar scope stops writing at once.
#[tokio::test]
async fn a_delete_retires_the_database_before_it_waits_for_users() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8, 0);
    let acme = key(&fleet, "acme");
    let db = fleet.open(&acme).await.unwrap();
    let busy = db.handle().checkout();
    let retired = std::sync::Arc::clone(&db.retired);
    drop(db);
    let deleting = {
        let (fleet, acme) = (fleet.clone(), acme.clone());
        tokio::spawn(async move { fleet.delete(&acme).await })
    };
    eventually(|| retired.load(std::sync::atomic::Ordering::Acquire)).await;
    assert!(
        !deleting.is_finished(),
        "the delete still waits for the user"
    );
    drop(busy);
    assert!(deleting.await.unwrap().unwrap());
}

/// Keeps the database it saw in `on_open`.
#[derive(Default)]
struct Keeper(std::sync::Mutex<Option<super::FleetDatabase<TextEngine>>>);

impl super::FleetLifecycle<TextEngine> for Keeper {
    fn on_open(&self, db: &super::FleetDatabase<TextEngine>) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(db.clone());
        Ok(())
    }
}

struct Refuse;

impl super::FleetLifecycle<TextEngine> for Refuse {
    fn on_open(&self, _: &super::FleetDatabase<TextEngine>) -> Result<(), String> {
        Err("no".to_owned())
    }
}

/// A refused open retires the database that an earlier hook kept.
#[tokio::test]
async fn a_refused_open_retires_what_an_earlier_hook_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8, 0);
    let keeper = std::sync::Arc::new(Keeper::default());
    fleet.add_lifecycle(std::sync::Arc::clone(&keeper) as _);
    fleet.add_lifecycle(std::sync::Arc::new(Refuse));
    let error = fleet.open(&key(&fleet, "acme")).await.unwrap_err();
    assert!(matches!(error, FleetError::Unavailable { .. }), "{error}");
    let kept = keeper.0.lock().unwrap().take().expect("the first hook ran");
    assert!(kept.retired.load(std::sync::atomic::Ordering::Acquire));
}

/// A fence retires a database that a close still drains: the close waits
/// for a user, and the host loses the slot meanwhile.
#[tokio::test]
async fn a_fence_retires_a_database_that_is_still_draining() {
    use crate::db::fleet_ownership::{FleetOwnership, MemoryOwnership, OwnerId, SlotRange};
    use std::sync::Arc;
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8, 0);
    let clock = crate::time::TickingClock::starting_at(
        chrono::DateTime::from_timestamp_millis(1_000_000).unwrap(),
    );
    let store = Arc::new(MemoryOwnership::new(Arc::new(clock.clone())));
    let host = |owner: &str| {
        Arc::new(
            FleetOwnership::new(
                Arc::clone(&store) as _,
                OwnerId::new(owner).unwrap(),
                vec![SlotRange::ALL],
                Duration::from_secs(30),
            )
            .unwrap(),
        )
    };
    let a = host("host-a");
    assert_eq!(
        fleet
            .start_ownership(Arc::clone(&a))
            .await
            .unwrap()
            .claimed
            .len(),
        1
    );
    let acme = key(&fleet, "acme");
    let db = fleet.open(&acme).await.unwrap();
    let busy = db.handle().checkout();
    let retired = Arc::clone(&db.retired);
    drop(db);
    let closing = {
        let (fleet, acme) = (fleet.clone(), acme.clone());
        tokio::spawn(async move { fleet.close(&acme).await })
    };
    eventually(|| fleet.stats().draining == 1).await;
    assert!(
        !retired.load(std::sync::atomic::Ordering::Acquire),
        "a plain close settles"
    );

    clock.advance(Duration::from_secs(31));
    assert_eq!(host("host-b").refresh().await.claimed.len(), 1);
    assert_eq!(a.refresh().await.lost, [SlotRange::ALL]);
    assert_eq!(fleet.fence_unowned(), 0, "nothing open to close");
    assert!(
        retired.load(std::sync::atomic::Ordering::Acquire),
        "the fence reaches the drain"
    );
    assert!(!closing.is_finished());
    drop(busy);
    closing.await.unwrap();
}

#[tokio::test]
async fn a_rebuild_engine_with_no_replica_rebuilds_and_ships_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let fleet = fleet(tmp.path(), 8, 0);
    let acme = key(&fleet, "acme");
    let stats = fleet.stats();
    assert_eq!(stats.ship_mode, "rebuild");
    assert_eq!(stats.rpo_secs, None);
    // Nothing on disk: nothing to rebuild.
    assert!(matches!(
        fleet.restore_latest(&acme).await,
        Err(FleetError::NotFound { .. })
    ));
    let db = fleet.open(&acme).await.unwrap();
    TextEngine::write(db.handle(), "derived").unwrap();
    drop(db);
    assert!(matches!(
        fleet.ship_now(&acme).await,
        Err(FleetError::NotReplicated { .. })
    ));
    fleet.restore_latest(&acme).await.unwrap();
    let db = fleet.open(&acme).await.unwrap();
    assert_eq!(TextEngine::read(db.handle()).unwrap(), ["rebuilt"]);
    drop(db);
    fleet.close_all().await;
}

#[tokio::test]
async fn a_generations_engine_with_no_replica_restores_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let mut engine = TextEngine::new(100);
    engine.every = Some(Duration::from_secs(60));
    let fleet = DatabaseFleet::with_engine(&config(tmp.path(), 8, 0), engine).unwrap();
    let acme = key(&fleet, "acme");
    assert_eq!(fleet.stats().ship_mode, "generations");
    assert_eq!(fleet.stats().rpo_secs, None, "no replica: no RPO");
    assert!(matches!(
        fleet.restore_latest(&acme).await,
        Err(FleetError::NotReplicated { .. })
    ));
    assert!(matches!(
        fleet.ship_now(&acme).await,
        Err(FleetError::NotReplicated { .. })
    ));
}

#[test]
fn a_fenced_ship_answers_421() {
    let error = FleetError::Fenced {
        name: "tenant:acme".to_owned(),
        epoch: 1,
        newest: 2,
    };
    assert_eq!(error.http_status(), http::StatusCode::MISDIRECTED_REQUEST);
    assert!(error.to_string().contains("epoch 2"));
    assert_eq!(super::ShipReport::default().generation, None);
}
