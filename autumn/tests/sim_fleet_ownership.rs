//! A partitioned old owner, in virtual time (issue #3233).
//!
//! Two hosts, each with its own volume, share the ownership table and the
//! replicas. Host A owns every slot. Then A loses the ownership table but
//! stays alive. The test checks that:
//!
//! - A stops serving when its local lease runs out, before the store lets
//!   another host claim.
//! - B claims only after A's lease lapsed, at a new epoch, and serves the
//!   data from A's replica.
//! - At no step do both hosts serve one slot.
//! - When the partition heals, A finds the range lost and does not serve.
//!
//! Standalone `[[test]]` binary, run in the single-threaded sim step:
//! `cargo test -p autumn-web --features "sqlite,test-support" --test sim_fleet_ownership -- --test-threads=1`.

#![cfg(all(feature = "sqlite", feature = "test-support"))]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use autumn_web::config::{DatabaseConfig, DatabaseFleetConfig};
use autumn_web::db::fleet::{DatabaseFleet, FleetDatabase, FleetError};
use autumn_web::db::fleet_ownership::{
    FleetOwnership, Grant, OwnerId, OwnershipError, OwnershipRow, SlotOwnershipStore, SlotRange,
    SqlSlotOwnership,
};
use autumn_web::db::fleet_replication::FleetReplication;
use autumn_web::fleet_layout::FleetMode;
use autumn_web::lock::FencingToken;
use autumn_web::reexports::{diesel, diesel_async};
use autumn_web::replication::{FileDestination, ReplicaDestination, ReplicationSettings};
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::time::AmbientClock;
use futures::future::BoxFuture;
use tokio_util::sync::CancellationToken;

const TTL: Duration = Duration::from_secs(30);

/// The ownership table as one host sees it: the network to it can be cut.
struct Link {
    store: SqlSlotOwnership,
    cut: AtomicBool,
}

impl Link {
    fn reach(&self) -> Result<(), OwnershipError> {
        if self.cut.load(Ordering::SeqCst) {
            Err(OwnershipError::Store("partitioned".to_owned()))
        } else {
            Ok(())
        }
    }
}

impl SlotOwnershipStore for Link {
    fn claim<'a>(
        &'a self,
        range: SlotRange,
        owner: &'a OwnerId,
        ttl: Duration,
        floor: u64,
    ) -> BoxFuture<'a, Result<Grant, OwnershipError>> {
        Box::pin(async move {
            self.reach()?;
            self.store.claim(range, owner, ttl, floor).await
        })
    }

    fn renew<'a>(
        &'a self,
        range: SlotRange,
        owner: &'a OwnerId,
        epoch: FencingToken,
        ttl: Duration,
    ) -> BoxFuture<'a, Result<Grant, OwnershipError>> {
        Box::pin(async move {
            self.reach()?;
            self.store.renew(range, owner, epoch, ttl).await
        })
    }

    fn release<'a>(
        &'a self,
        range: SlotRange,
        owner: &'a OwnerId,
        epoch: FencingToken,
    ) -> BoxFuture<'a, Result<bool, OwnershipError>> {
        Box::pin(async move {
            self.reach()?;
            self.store.release(range, owner, epoch).await
        })
    }

    fn rows(&self) -> BoxFuture<'_, Result<Vec<OwnershipRow>, OwnershipError>> {
        self.store.rows()
    }
}

struct Host {
    fleet: DatabaseFleet,
    replication: Arc<FleetReplication>,
    ownership: Arc<FleetOwnership>,
    link: Arc<Link>,
    stop: CancellationToken,
    renewals: tokio::task::JoinHandle<()>,
}

async fn host(dir: &std::path::Path, name: &str) -> Host {
    let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
        mode: FleetMode::Tenant,
        root: dir.join(name).display().to_string(),
        path: None,
        max_open: 8,
        pool_size: 2,
        create_on_demand: Some(true),
        idle_close_secs: 0,
        restore_missing: true,
        ownership: None,
        replica_retention_after_delete: std::time::Duration::ZERO,
        restore_check_sample: 0,
        replication_workers: 4,
        min_residency: std::time::Duration::from_secs(2),
    })
    .build()
    .unwrap();
    let destination: Arc<dyn ReplicaDestination> =
        Arc::new(FileDestination::new(dir.join("replicas")).unwrap());
    let replication = Arc::new(FleetReplication::new(
        destination,
        ReplicationSettings {
            database_path: std::path::PathBuf::new(),
            root: "app/sim".to_owned(),
            sync_interval: Duration::from_secs(1),
            snapshot_interval: Duration::from_secs(3600),
            max_wal_bytes: 4 * 1024 * 1024,
            retention: Duration::from_secs(24 * 3600),
            verify_interval: None,
        },
        Duration::from_secs(30),
        true,
        Arc::new(AmbientClock),
    ));
    assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
    // The shared ownership table, on the sim clock.
    let control = autumn_web::db::create_pool(&DatabaseConfig {
        url: Some(format!("sqlite://{}", dir.join("control.db").display())),
        ..Default::default()
    })
    .unwrap()
    .unwrap();
    let store = SqlSlotOwnership::new(control, Arc::new(AmbientClock));
    store.ensure_schema().await.unwrap();
    let link = Arc::new(Link {
        store,
        cut: AtomicBool::new(false),
    });
    let ownership = Arc::new(
        FleetOwnership::new(
            Arc::clone(&link) as Arc<dyn SlotOwnershipStore>,
            OwnerId::new(name).unwrap(),
            vec![SlotRange::ALL],
            TTL,
        )
        .unwrap(),
    );
    // As boot does: install and claim, then run the loop.
    assert!(fleet.start_ownership(Arc::clone(&ownership)).await.is_ok());
    let stop = CancellationToken::new();
    let renewals = fleet.spawn_ownership(stop.clone());
    Host {
        fleet,
        replication,
        ownership,
        link,
        stop,
        renewals,
    }
}

async fn exec(db: &FleetDatabase, sql: &str) -> Result<usize, String> {
    let mut conn = db.pool().get().await.map_err(|e| e.to_string())?;
    diesel_async::RunQueryDsl::execute(diesel::sql_query(sql), &mut *conn)
        .await
        .map_err(|e| e.to_string())
}

async fn count(db: &FleetDatabase) -> i64 {
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        n: i64,
    }
    let mut conn = db.pool().get().await.unwrap();
    diesel_async::RunQueryDsl::get_result::<Count>(
        diesel::sql_query("SELECT COUNT(*) AS n FROM notes"),
        &mut *conn,
    )
    .await
    .unwrap()
    .n
}

/// On the sim thread: the replicator then reads the sim clock.
fn tick(replication: &FleetReplication) {
    replication.tick_all();
}

/// Let the ownership loops run their step for this instant. Their store
/// calls run on blocking threads, so poll in 1 ms steps until `done`.
async fn settle(done: impl Fn() -> bool) {
    for _ in 0..5_000 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("the ownership loops did not settle");
}

/// The safety property: at most one host serves the slot.
fn at_most_one_serves(a: &Host, b: &Host, slot: u16) {
    assert!(
        a.ownership.epoch_for(slot).is_none() || b.ownership.epoch_for(slot).is_none(),
        "two hosts serve slot {slot}"
    );
}

#[sim_test]
async fn sim_a_partitioned_old_owner_is_fenced_before_the_new_owner_serves(sim: Sim) {
    let dir = tempfile::tempdir().unwrap();
    let a = host(dir.path(), "a").await;
    let b = host(dir.path(), "b").await;
    let key = a.fleet.key_for("acme").unwrap();
    let slot = key.routing_slot();
    assert_eq!(
        a.ownership.held(),
        vec![(SlotRange::ALL, FencingToken::try_from(1).unwrap())]
    );
    assert_eq!(b.ownership.last_report().waiting, vec![SlotRange::ALL]);

    // A serves and replicates.
    let held = a.fleet.open(&key).await.unwrap();
    exec(&held, "CREATE TABLE notes (body TEXT NOT NULL)")
        .await
        .unwrap();
    exec(&held, "INSERT INTO notes VALUES ('a1')")
        .await
        .unwrap();
    tick(&a.replication);
    at_most_one_serves(&a, &b, slot);

    // A loses the ownership table; it still runs and holds its database.
    a.link.cut.store(true, Ordering::SeqCst);

    // t = 10 s: A's renew fails, its local lease still runs.
    sim.advance(Duration::from_secs(10)).await;
    settle(|| !a.ownership.last_report().failed.is_empty()).await;
    assert!(a.ownership.epoch_for(slot).is_some());
    at_most_one_serves(&a, &b, slot);

    // t = 20 s: A's local lease runs out. It closes the database and
    // refuses new opens, while the store still holds its lease.
    sim.advance(Duration::from_secs(10)).await;
    settle(|| a.ownership.epoch_for(slot).is_none() && held.pool().is_closed()).await;
    assert_eq!(a.fleet.stats().open, 0, "the old owner's database closed");
    assert!(
        exec(&held, "INSERT INTO notes VALUES ('late')")
            .await
            .is_err()
    );
    assert!(matches!(
        a.fleet.open(&key).await,
        Err(FleetError::NotOwner { .. })
    ));
    // B asks now, inside the window: the store still refuses it.
    assert_eq!(
        b.ownership.refresh().await.waiting,
        vec![SlotRange::ALL],
        "A's lease is live in the store"
    );
    assert!(b.ownership.epoch_for(slot).is_none());
    at_most_one_serves(&a, &b, slot);

    // t = 30 s: the store lease lapses. B claims at epoch 2 and serves the
    // data from A's replica.
    sim.advance(Duration::from_secs(10)).await;
    settle(|| b.ownership.epoch_for(slot).is_some()).await;
    at_most_one_serves(&a, &b, slot);
    let db = b.fleet.open(&key).await.unwrap();
    assert_eq!(db.epoch(), Some(FencingToken::try_from(2).unwrap()));
    assert_eq!(count(&db).await, 1, "restored from A's replica");
    exec(&db, "INSERT INTO notes VALUES ('b1')").await.unwrap();
    tick(&b.replication);

    // The partition heals: A's renew is refused, and it does not serve.
    a.link.cut.store(false, Ordering::SeqCst);
    sim.advance(Duration::from_secs(10)).await;
    settle(|| a.ownership.held().is_empty()).await;
    assert_eq!(a.ownership.last_report().lost, vec![SlotRange::ALL]);
    assert!(matches!(
        a.fleet.open(&key).await,
        Err(FleetError::NotOwner { .. })
    ));
    at_most_one_serves(&a, &b, slot);
    assert_eq!(count(&db).await, 2);

    drop(db);
    for host in [a, b] {
        host.stop.cancel();
        host.renewals.await.unwrap();
        host.fleet.close_all().await;
    }
}
