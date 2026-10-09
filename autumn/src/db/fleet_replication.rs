//! Continuous replication of every open fleet database (ADR 0019 §5).
//!
//! A cloud host attaches one volume, so the volume cannot be how a fleet
//! database survives the host or moves to another one. Object storage is the
//! medium between volumes: each fleet database ships its WAL there under
//! `<root>/fleet/tenant/<id>` or `<root>/fleet/slot/<nnnnn>`, through the same
//! engine ([`Replicator`]) and destination as the control database.
//!
//! ```text
//! open    ─► register a Replicator (its own connection, its own status)
//! loop    ─► every sync_interval, tick each registered Replicator
//! close   ─► pool drained ─► final tick (ships the last frames) ─► drop
//!                             └► only now may SQLite's last-connection
//!                                checkpoint run, with nothing unshipped
//! missing ─► restore_missing = true: rebuild the file from its replica
//!            before the open, so a fresh volume serves the database
//! ```
//!
//! The databases run with `wal_autocheckpoint = 0`; the replicator is the only
//! checkpointer, and it checkpoints only what is already offsite. That is the
//! control database's invariant, applied per database.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::actuator::{HealthCheckOutput, HealthIndicator};
use crate::fleet_layout::FleetDbKey;
use crate::replication::destination::ReplicaDestination;
use crate::replication::engine::{ReplicationSettings, Replicator};
use crate::replication::restore::{RestoreError, RestoreOutcome};
use crate::replication::status::ReplicationStatus;
use crate::time::ClockSource;

use super::fleet::{FleetDatabase, FleetLifecycle};

/// Health indicator name of the fleet's replication.
pub const FLEET_REPLICATION_INDICATOR: &str = "replication:fleet";

/// How often the loop checks for shutdown while it sleeps.
const STOP_POLL: Duration = Duration::from_millis(100);

/// Most ticks the close-time ship takes before it gives up and parks the
/// replicator. One tick ships at most one segment, so a backlog can take
/// several.
const FINAL_SHIP_MAX_TICKS: usize = 64;

/// The destination key prefix of one fleet database, under the control
/// database's replication root.
#[must_use]
pub fn replica_root(base: &str, key: &FleetDbKey) -> String {
    let base = base.trim_end_matches('/');
    match key {
        FleetDbKey::Tenant { id, .. } => format!("{base}/fleet/tenant/{id}"),
        FleetDbKey::Slot(slot) => format!("{base}/fleet/slot/{slot:05}"),
    }
}

struct ActiveReplica {
    replicator: Mutex<Replicator>,
    status: Arc<ReplicationStatus>,
    registered_at: DateTime<Utc>,
}

/// Replicates the open databases of one fleet. Installed by
/// [`DatabaseFleet::install_replication`](super::fleet::DatabaseFleet::install_replication).
pub struct FleetReplication {
    destination: Arc<dyn ReplicaDestination>,
    /// The control database's settings: `root` is the base every fleet
    /// database's prefix hangs off; `database_path` is replaced per database.
    settings: ReplicationSettings,
    restore_missing: bool,
    lag_alert_after: Duration,
    clock: Arc<dyn ClockSource>,
    active: Mutex<HashMap<FleetDbKey, Arc<ActiveReplica>>>,
    /// Replicators of closed databases whose close-time ship did not catch
    /// up (a destination outage). Each keeps its own connection open, so
    /// `SQLite` cannot checkpoint the unshipped frames away; the loop keeps
    /// ticking them and drops each once it is caught up. A reopen takes its
    /// replicator back instead of starting a second one.
    parked: Mutex<HashMap<FleetDbKey, Arc<ActiveReplica>>>,
    stopped: AtomicBool,
}

impl FleetReplication {
    /// Replicate fleet databases next to the control database.
    ///
    /// `settings` are the control database's resolved replication settings;
    /// `lag_alert_after` is the lag at which the health indicator turns
    /// `Down`; `restore_missing` rebuilds a missing database from its replica
    /// before opening it.
    #[must_use]
    pub fn new(
        destination: Arc<dyn ReplicaDestination>,
        settings: ReplicationSettings,
        lag_alert_after: Duration,
        restore_missing: bool,
        clock: Arc<dyn ClockSource>,
    ) -> Self {
        Self {
            destination,
            settings,
            restore_missing,
            lag_alert_after,
            clock,
            active: Mutex::new(HashMap::new()),
            parked: Mutex::new(HashMap::new()),
            stopped: AtomicBool::new(false),
        }
    }

    fn root_for(&self, key: &FleetDbKey) -> String {
        replica_root(&self.settings.root, key)
    }

    fn active(&self) -> std::sync::MutexGuard<'_, HashMap<FleetDbKey, Arc<ActiveReplica>>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn parked(&self) -> std::sync::MutexGuard<'_, HashMap<FleetDbKey, Arc<ActiveReplica>>> {
        self.parked.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Drop a parked replicator that is still the one parked for `key`.
    fn retire_parked(&self, key: &FleetDbKey, replica: &Arc<ActiveReplica>) -> bool {
        let mut parked = self.parked();
        let current = parked
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, replica));
        if current {
            parked.remove(key);
        }
        current
    }

    /// Closed databases still shipping their backlog.
    #[must_use]
    pub fn closing(&self) -> usize {
        self.parked().len()
    }

    fn all_replicas(&self) -> Vec<(FleetDbKey, Arc<ActiveReplica>)> {
        let mut replicas: Vec<(FleetDbKey, Arc<ActiveReplica>)> = self
            .active()
            .iter()
            .map(|(key, replica)| (key.clone(), Arc::clone(replica)))
            .collect();
        replicas.extend(
            self.parked()
                .iter()
                .map(|(key, replica)| (key.clone(), Arc::clone(replica))),
        );
        replicas
    }

    /// Databases replicating now.
    #[must_use]
    pub fn replicating(&self) -> usize {
        self.active().len()
    }

    /// Tick every registered replicator once, and retire each parked one
    /// that has caught up. Blocking. The loop calls this; tests call it to
    /// step replication without sleeping.
    pub fn tick_all(&self) {
        let active: Vec<(FleetDbKey, Arc<ActiveReplica>)> = self
            .active()
            .iter()
            .map(|(key, replica)| (key.clone(), Arc::clone(replica)))
            .collect();
        for (key, replica) in active {
            let mut replicator = replica
                .replicator
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Err(error) = replicator.tick() {
                tracing::warn!(database = %key, error = %error, "fleet replication tick failed");
            }
        }
        let parked: Vec<(FleetDbKey, Arc<ActiveReplica>)> = self
            .parked()
            .iter()
            .map(|(key, replica)| (key.clone(), Arc::clone(replica)))
            .collect();
        for (key, replica) in parked {
            let caught_up = {
                let mut replicator = replica
                    .replicator
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                ship_until_caught_up(&mut replicator).is_ok()
            };
            if caught_up && self.retire_parked(&key, &replica) {
                tracing::info!(database = %key, "fleet replication: closed database caught up");
            }
        }
    }

    /// Run the loop on a dedicated thread until [`stop`](Self::stop). The
    /// final ship of each database happens when it closes, not here.
    ///
    /// # Errors
    ///
    /// The OS refused to start the thread.
    pub fn spawn_loop(self: &Arc<Self>) -> std::io::Result<std::thread::JoinHandle<()>> {
        let this = Arc::clone(self);
        std::thread::Builder::new()
            .name("autumn-fleet-replication".to_owned())
            .spawn(move || {
                while !this.stopped.load(Ordering::Acquire) {
                    this.tick_all();
                    let mut slept = Duration::ZERO;
                    while slept < this.settings.sync_interval
                        && !this.stopped.load(Ordering::Acquire)
                    {
                        let step = STOP_POLL.min(this.settings.sync_interval.saturating_sub(slept));
                        std::thread::sleep(step);
                        slept += step;
                    }
                }
            })
    }

    /// Stop the loop after its current tick.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// Rebuild `path` from the replica of `key`, as of `target` (latest when
    /// `None`). Blocking. The database must be closed.
    ///
    /// # Errors
    ///
    /// [`RestoreError::NoReplica`] when nothing was ever shipped for `key`, or
    /// any other restore failure.
    pub fn restore(
        &self,
        key: &FleetDbKey,
        target: Option<DateTime<Utc>>,
        path: &Path,
    ) -> Result<RestoreOutcome, RestoreError> {
        // A parked replicator of the old file must not outlive it.
        self.parked().remove(key);
        crate::replication::restore::restore(
            self.destination.as_ref(),
            &self.root_for(key),
            target,
            path,
        )
    }

    /// The health indicator for every replicating database.
    #[must_use]
    pub fn indicator(self: &Arc<Self>) -> Arc<dyn HealthIndicator> {
        Arc::new(FleetReplicationIndicator {
            replication: Arc::clone(self),
        })
    }

    fn health(&self) -> HealthCheckOutput {
        let now = self.clock.now();
        let closing = self.closing();
        let replicas = self.all_replicas();
        let mut worst: Option<(Duration, String)> = None;
        let mut failing = 0_u64;
        let mut pending_bytes = 0_u64;
        for (key, replica) in &replicas {
            let snapshot = replica.status.snapshot();
            pending_bytes = pending_bytes.saturating_add(snapshot.pending_bytes);
            if snapshot.consecutive_failures > 0 {
                failing += 1;
            }
            // Before the first successful tick, lag runs from registration, so
            // a database that never ships still trips the threshold.
            let lag = snapshot.lag(now).unwrap_or_else(|| {
                now.signed_duration_since(replica.registered_at)
                    .to_std()
                    .unwrap_or(Duration::ZERO)
            });
            if worst.as_ref().is_none_or(|(w, _)| lag > *w) {
                worst = Some((lag, key.name()));
            }
        }
        let mut details: HashMap<String, serde_json::Value> = HashMap::new();
        details.insert("databases".to_owned(), replicas.len().into());
        details.insert("closing".to_owned(), closing.into());
        details.insert("failing".to_owned(), failing.into());
        details.insert("pending_bytes".to_owned(), pending_bytes.into());
        details.insert(
            "lag_alert_after_seconds".to_owned(),
            self.lag_alert_after.as_secs().into(),
        );
        details.insert("destination".to_owned(), self.destination.describe().into());
        let healthy = if let Some((lag, name)) = worst {
            details.insert("worst_lag_seconds".to_owned(), lag.as_secs().into());
            details.insert("worst_database".to_owned(), name.into());
            lag <= self.lag_alert_after
        } else {
            true
        };
        if healthy {
            HealthCheckOutput::up().with_details(details)
        } else {
            HealthCheckOutput::down().with_details(details)
        }
    }
}

impl FleetLifecycle for FleetReplication {
    fn restore_missing(&self, key: &FleetDbKey, path: &Path) -> Result<bool, String> {
        if !self.restore_missing {
            return Ok(false);
        }
        match self.restore(key, None, path) {
            Ok(outcome) => {
                tracing::info!(
                    database = %key,
                    bytes = outcome.bytes,
                    "fleet database restored from its replica"
                );
                Ok(true)
            }
            Err(RestoreError::NoReplica { .. }) => Ok(false),
            Err(error) => Err(format!("restore from replica failed: {error}")),
        }
    }

    fn on_open(&self, db: &FleetDatabase) -> Result<(), String> {
        // A closed database still shipping its backlog keeps its replicator:
        // two replicators on one file would both checkpoint it.
        let parked = self.parked().remove(db.key());
        if let Some(parked) = parked {
            self.active().insert(db.key().clone(), parked);
            return Ok(());
        }
        let settings = ReplicationSettings {
            database_path: db.path().to_path_buf(),
            root: self.root_for(db.key()),
            // A verification restores the whole database; across a fleet that
            // is the operator's call (`restore`), not a timer's.
            verify_interval: None,
            ..self.settings.clone()
        };
        let status = Arc::new(ReplicationStatus::new(self.destination.describe()));
        let replicator =
            Replicator::new(settings, Arc::clone(&self.destination), Arc::clone(&status))
                .with_clock(Arc::clone(&self.clock));
        self.active().insert(
            db.key().clone(),
            Arc::new(ActiveReplica {
                replicator: Mutex::new(replicator),
                status,
                registered_at: self.clock.now(),
            }),
        );
        Ok(())
    }

    fn on_close(&self, db: &FleetDatabase) {
        let Some(replica) = self.active().remove(db.key()) else {
            return;
        };
        // Every pooled connection is gone. Ship everything they committed
        // before the replicator's own connection — the last one — closes and
        // lets SQLite checkpoint.
        let shipped = {
            let mut replicator = replica
                .replicator
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            ship_until_caught_up(&mut replicator)
        };
        if let Err(error) = shipped {
            tracing::warn!(
                database = %db.key(),
                error = %error,
                "fleet replication: the close-time ship did not catch up; the replicator \
                 stays open and keeps shipping until it does"
            );
            self.parked().insert(db.key().clone(), replica);
        }
    }

    fn on_delete(&self, key: &FleetDbKey) {
        // The files are about to go: a parked replicator must not hold them.
        self.parked().remove(key);
    }
}

/// Tick until nothing committed is left unshipped.
///
/// # Errors
///
/// A tick failed, made no progress, or the backlog outlasted
/// [`FINAL_SHIP_MAX_TICKS`].
fn ship_until_caught_up(replicator: &mut Replicator) -> Result<(), String> {
    for _ in 0..FINAL_SHIP_MAX_TICKS {
        let report = replicator.tick().map_err(|e| e.to_string())?;
        if report.pending_bytes == 0 {
            return Ok(());
        }
        let progressed = report.segments > 0 || report.snapshot_taken || report.checkpointed;
        if !progressed {
            return Err(format!(
                "{} WAL bytes are pending and the last tick shipped nothing",
                report.pending_bytes
            ));
        }
    }
    Err(format!("still behind after {FINAL_SHIP_MAX_TICKS} ticks"))
}

struct FleetReplicationIndicator {
    replication: Arc<FleetReplication>,
}

impl HealthIndicator for FleetReplicationIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(std::future::ready(self.replication.health()))
    }

    fn group(&self) -> crate::actuator::IndicatorGroup {
        crate::actuator::IndicatorGroup::HealthOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DatabaseFleetConfig;
    use crate::db::fleet::{DatabaseFleet, FleetError};
    use crate::fleet_layout::FleetMode;
    use crate::replication::FileDestination;

    fn settings() -> ReplicationSettings {
        ReplicationSettings {
            database_path: std::path::PathBuf::new(),
            root: "app/test".to_owned(),
            sync_interval: Duration::from_secs(1),
            snapshot_interval: Duration::from_secs(3600),
            max_wal_bytes: 4 * 1024 * 1024,
            retention: Duration::from_secs(24 * 3600),
            verify_interval: None,
        }
    }

    fn replicated_fleet(
        root: &Path,
        replicas: &Path,
        restore_missing: bool,
    ) -> (DatabaseFleet, Arc<FleetReplication>) {
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            mode: FleetMode::Tenant,
            root: root.display().to_string(),
            path: None,
            max_open: 8,
            pool_size: 2,
            create_on_demand: Some(true),
            idle_close_secs: 0,
            restore_missing,
        })
        .build()
        .unwrap();
        let destination: Arc<dyn ReplicaDestination> =
            Arc::new(FileDestination::new(replicas).unwrap());
        let replication = Arc::new(FleetReplication::new(
            destination,
            settings(),
            Duration::from_secs(30),
            restore_missing,
            Arc::new(crate::time::SystemClock),
        ));
        assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
        assert!(fleet.install_replication(Arc::clone(&replication)).is_err());
        (fleet, replication)
    }

    async fn exec(db: &FleetDatabase, sql: &str) {
        use diesel_async::RunQueryDsl as _;
        let mut conn = db.pool().get().await.unwrap();
        diesel::sql_query(sql).execute(&mut *conn).await.unwrap();
    }

    async fn count(db: &FleetDatabase) -> i64 {
        use diesel_async::RunQueryDsl as _;
        #[derive(diesel::QueryableByName)]
        struct Count {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            n: i64,
        }
        let mut conn = db.pool().get().await.unwrap();
        diesel::sql_query("SELECT COUNT(*) AS n FROM notes")
            .get_result::<Count>(&mut *conn)
            .await
            .unwrap()
            .n
    }

    async fn wal_autocheckpoint(db: &FleetDatabase) -> i64 {
        use diesel_async::RunQueryDsl as _;
        #[derive(diesel::QueryableByName)]
        struct Value {
            #[diesel(sql_type = diesel::sql_types::BigInt)]
            wal_autocheckpoint: i64,
        }
        let mut conn = db.pool().get().await.unwrap();
        diesel::sql_query("PRAGMA wal_autocheckpoint")
            .get_result::<Value>(&mut *conn)
            .await
            .unwrap()
            .wal_autocheckpoint
    }

    #[test]
    fn replica_roots_hang_off_the_control_root() {
        assert_eq!(
            replica_root("app/prod/", &FleetDbKey::Slot(42)),
            "app/prod/fleet/slot/00042"
        );
        let key = FleetDbKey::Tenant {
            id: crate::fleet_layout::TenantDbId::parse("acme").unwrap(),
            slot: 1,
        };
        assert_eq!(replica_root("app/prod", &key), "app/prod/fleet/tenant/acme");
        for root in [
            replica_root("app/prod", &key),
            replica_root("app/prod", &FleetDbKey::Slot(0)),
        ] {
            crate::replication::destination::validate_key(&root).unwrap();
        }
    }

    #[tokio::test]
    async fn a_closed_database_ships_its_last_frames_and_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let replicas = tmp.path().join("replicas");
        let (fleet, replication) = replicated_fleet(&tmp.path().join("a"), &replicas, false);
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        assert_eq!(
            wal_autocheckpoint(&db).await,
            0,
            "the replicator checkpoints"
        );
        assert_eq!(replication.replicating(), 1);
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(&db, "INSERT INTO notes VALUES ('shipped by a tick')").await;
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.tick_all()
        })
        .await
        .unwrap();
        // Written after the last tick: only the close-time ship carries it.
        exec(&db, "INSERT INTO notes VALUES ('shipped on close')").await;
        drop(db);
        fleet.close(&key).await;
        assert_eq!(replication.replicating(), 0);

        // Lose the volume.
        let path = fleet.path_of(&key);
        std::fs::remove_file(&path).unwrap();
        for sidecar in crate::fleet_layout::sidecar_paths(&path) {
            let _ = std::fs::remove_file(sidecar);
        }
        let outcome = fleet.restore(&key, None).await.unwrap();
        assert!(outcome.bytes > 0);
        let restored = fleet.open(&key).await.unwrap();
        assert_eq!(count(&restored).await, 2);
    }

    #[tokio::test]
    async fn a_fresh_volume_serves_a_database_from_its_replica() {
        let tmp = tempfile::tempdir().unwrap();
        let replicas = tmp.path().join("replicas");
        let (old_host, _) = replicated_fleet(&tmp.path().join("old"), &replicas, false);
        let key = old_host.key_for("acme").unwrap();
        let db = old_host.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(&db, "INSERT INTO notes VALUES ('moved with the slot')").await;
        drop(db);
        old_host.close_all().await;

        // A second host with an empty volume and the same object storage.
        let (new_host, replication) = replicated_fleet(&tmp.path().join("new"), &replicas, true);
        assert!(!new_host.exists(&key));
        let db = new_host.open(&key).await.unwrap();
        assert_eq!(count(&db).await, 1);
        assert_eq!(new_host.stats().created_total, 0, "restored, not created");
        assert_eq!(replication.replicating(), 1, "and replicating again");

        // A key that never shipped is simply created.
        new_host.open_for("globex").await.unwrap();
        assert_eq!(new_host.stats().created_total, 1);
    }

    #[tokio::test]
    async fn restore_without_replication_or_replica_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = DatabaseFleet::builder(DatabaseFleetConfig {
            mode: FleetMode::Slot,
            root: tmp.path().join("plain").display().to_string(),
            path: None,
            max_open: 8,
            pool_size: 2,
            create_on_demand: None,
            idle_close_secs: 0,
            restore_missing: false,
        })
        .build()
        .unwrap();
        let key = FleetDbKey::Slot(1);
        assert!(matches!(
            plain.restore(&key, None).await,
            Err(FleetError::NotReplicated { .. })
        ));
        let db = plain.open(&key).await.unwrap();
        assert_eq!(
            wal_autocheckpoint(&db).await,
            1000,
            "a fleet nobody replicates keeps SQLite's own checkpointing"
        );

        let (fleet, _) =
            replicated_fleet(&tmp.path().join("r"), &tmp.path().join("replicas"), false);
        let never_shipped = fleet.key_for("nobody").unwrap();
        assert!(matches!(
            fleet.restore(&never_shipped, None).await,
            Err(FleetError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn the_indicator_reports_every_replicating_database() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication) =
            replicated_fleet(&tmp.path().join("a"), &tmp.path().join("replicas"), false);
        let output = replication.indicator().check().await;
        assert_eq!(output.status, crate::actuator::HealthStatus::Up);
        assert_eq!(output.details["databases"], 0);

        fleet.open_for("acme").await.unwrap();
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.tick_all()
        })
        .await
        .unwrap();
        let output = replication.indicator().check().await;
        assert_eq!(
            output.status,
            crate::actuator::HealthStatus::Up,
            "{:?}",
            output.details
        );
        assert_eq!(output.details["databases"], 1);
        assert_eq!(output.details["failing"], 0);
        assert_eq!(output.details["worst_database"], "tenant:acme");
        fleet.close_all().await;
    }

    /// A destination that can be switched off, for an outage mid-close.
    struct Flaky {
        inner: FileDestination,
        down: AtomicBool,
    }

    impl Flaky {
        fn check(&self) -> Result<(), crate::replication::DestinationError> {
            if self.down.load(Ordering::SeqCst) {
                Err(crate::replication::DestinationError::Io {
                    op: "put",
                    detail: "destination is down".to_owned(),
                })
            } else {
                Ok(())
            }
        }
    }

    impl ReplicaDestination for Flaky {
        fn describe(&self) -> String {
            "flaky".to_owned()
        }
        fn put(&self, key: &str, body: &[u8]) -> Result<(), crate::replication::DestinationError> {
            self.check()?;
            self.inner.put(key, body)
        }
        fn put_file(
            &self,
            key: &str,
            path: &Path,
        ) -> Result<(), crate::replication::DestinationError> {
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

    #[tokio::test]
    async fn a_close_during_an_outage_keeps_shipping_until_caught_up() {
        let tmp = tempfile::tempdir().unwrap();
        let fleet = DatabaseFleet::builder(DatabaseFleetConfig {
            mode: FleetMode::Tenant,
            root: tmp.path().join("a").display().to_string(),
            path: None,
            max_open: 8,
            pool_size: 2,
            create_on_demand: Some(true),
            idle_close_secs: 0,
            restore_missing: false,
        })
        .build()
        .unwrap();
        let flaky = Arc::new(Flaky {
            inner: FileDestination::new(tmp.path().join("replicas")).unwrap(),
            down: AtomicBool::new(true),
        });
        let replication = Arc::new(FleetReplication::new(
            Arc::clone(&flaky) as Arc<dyn ReplicaDestination>,
            settings(),
            Duration::from_secs(30),
            false,
            Arc::new(crate::time::SystemClock),
        ));
        assert!(fleet.install_replication(Arc::clone(&replication)).is_ok());
        let key = fleet.key_for("acme").unwrap();
        let db = fleet.open(&key).await.unwrap();
        exec(&db, "CREATE TABLE notes (body TEXT NOT NULL)").await;
        exec(
            &db,
            "INSERT INTO notes VALUES ('written during the outage')",
        )
        .await;
        drop(db);
        fleet.close(&key).await;
        assert_eq!(replication.replicating(), 0);
        assert_eq!(replication.closing(), 1, "parked, not dropped");
        let health = replication.indicator().check().await;
        assert_eq!(health.details["closing"], 1);

        // Reopening takes the parked replicator back: one per file.
        let db = fleet.open(&key).await.unwrap();
        assert_eq!((replication.replicating(), replication.closing()), (1, 0));
        drop(db);
        fleet.close(&key).await;
        assert_eq!(replication.closing(), 1);

        // The destination recovers; the loop ships the backlog and retires it.
        flaky.down.store(false, Ordering::SeqCst);
        crate::time::spawn_blocking({
            let replication = Arc::clone(&replication);
            move || replication.tick_all()
        })
        .await
        .unwrap();
        assert_eq!(replication.closing(), 0);

        // What was written during the outage is in the replica.
        let path = fleet.path_of(&key);
        std::fs::remove_file(&path).unwrap();
        fleet.restore(&key, None).await.unwrap();
        assert_eq!(count(&fleet.open(&key).await.unwrap()).await, 1);
    }

    #[tokio::test]
    async fn a_failed_restore_releases_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let replicas = tmp.path().join("replicas");
        let (fleet, _) = replicated_fleet(&tmp.path().join("a"), &replicas, false);
        let key = fleet.key_for("acme").unwrap();
        // A file where the bucket directory belongs: creating it fails.
        std::fs::write(
            fleet.root().join(format!("{:03}", key.bucket())),
            b"not a directory",
        )
        .unwrap();
        let err = fleet.restore(&key, None).await.unwrap_err();
        assert!(matches!(err, FleetError::Io { .. }), "{err}");
        // The key is not stranded on a drain that never finishes.
        let reopened = tokio::time::timeout(Duration::from_secs(5), fleet.open(&key)).await;
        assert!(
            reopened.is_ok(),
            "open must not hang after a failed restore"
        );
    }

    #[tokio::test]
    async fn the_loop_ticks_until_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let (fleet, replication) =
            replicated_fleet(&tmp.path().join("a"), &tmp.path().join("replicas"), false);
        fleet.open_for("acme").await.unwrap();
        let handle = replication.spawn_loop().unwrap();
        for _ in 0..100 {
            let output = replication.indicator().check().await;
            if output.details.contains_key("worst_lag_seconds")
                && output.details["failing"] == 0
                && tmp
                    .path()
                    .join("replicas/app/test/fleet/tenant/acme")
                    .exists()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            tmp.path()
                .join("replicas/app/test/fleet/tenant/acme")
                .exists()
        );
        replication.stop();
        handle.join().unwrap();
        fleet.close_all().await;
    }
}
