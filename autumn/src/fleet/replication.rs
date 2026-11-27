//! The boot-time replication of a fleet, for every engine.
//!
//! Core builds the `[replication]` destination at boot, before the startup
//! hooks run. [`install_replication`] connects one fleet to it.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::AppState;
use crate::config::DatabaseFleetConfig;
use crate::db::fleet::DatabaseFleet;
use crate::db::fleet_engine::FleetEngine;
use crate::db::fleet_replication::{FLEET_REPLICATION_INDICATOR, FleetReplication};
use crate::replication::{ReplicaDestination, ReplicationSettings};

/// What [`install_replication`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReplicationInstall {
    /// The fleet ships to the `[replication]` destination.
    Installed,
    /// `[replication]` is off. Nothing ships.
    NoReplication,
}

/// Why [`install_replication`] did not install.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstallReplicationError {
    /// The fleet replicates already.
    #[error("the fleet already replicates")]
    AlreadyReplicates,
    /// Another fleet ships to this replica root. Two fleets with one tenant
    /// id write the same keys.
    #[error(
        "another fleet ({claimed_by}) already ships to the replica root {root:?}; one replica \
         root holds one fleet"
    )]
    RootInUse {
        /// The replica key root.
        root: String,
        /// The fleet root of the fleet that ships there.
        claimed_by: String,
    },
    /// The OS did not start the replication thread. Stop the boot.
    #[error("could not start the fleet replication thread")]
    Thread(#[source] std::io::Error),
}

/// Holds what `[replication]` built at boot. Core puts it in the app state.
pub struct ReplicaSource {
    destination: Arc<dyn ReplicaDestination>,
    settings: ReplicationSettings,
    lag_alert_after: Duration,
    final_flush: Duration,
    /// The root of the fleet that ships here. One fleet per source.
    claimed: Mutex<Option<PathBuf>>,
}

impl ReplicaSource {
    /// The source of the `[replication]` runtime core built.
    pub(crate) fn from_runtime(
        runtime: &crate::replication::ReplicationRuntime,
        lag_alert_after: Duration,
        final_flush: Duration,
    ) -> Self {
        Self {
            destination: Arc::clone(runtime.replicator.destination()),
            settings: runtime.settings.clone(),
            lag_alert_after,
            final_flush,
            claimed: Mutex::new(None),
        }
    }

    /// Puts the source in `state`.
    pub(crate) fn install(self, state: &AppState) {
        state.insert_extension(self);
    }
}

/// Connects `fleet` to the `[replication]` destination of the app.
///
/// An engine plugin calls it from its `on_startup` hook, after
/// `DatabaseFleet::with_engine` and before the first open:
///
/// ```rust,ignore
/// let fleet = DatabaseFleet::with_engine(&config, DuckDbEngine::new())?;
/// autumn_web::fleet::install_replication(&state, &fleet, &config)?;
/// state.insert_extension(fleet);
/// ```
///
/// It builds the [`FleetReplication`] from `[replication]` and from
/// `config` (`restore_missing`, `replica_retention_after_delete`,
/// `restore_check_sample`, `replication_workers`). It keeps the tombstones
/// on the control database, installs the replication on the fleet,
/// registers the `replication:fleet` indicator and starts the loop. The
/// `SQLite` fleet of `[database.fleet]` uses the same function.
///
/// Call it at boot, before the first open. `config` is the section the fleet
/// was built from.
///
/// With `[replication]` off it installs nothing. When `config` turns on
/// ownership, it then logs one warning: a standby that becomes the owner
/// opens empty databases. When `restore_missing` is on, it also logs a
/// warning. A one-off task (`autumn task`) builds no
/// `[replication]`, so there it returns [`ReplicationInstall::NoReplication`].
///
/// Shutdown stays with the plugin. Start `fleet.spawn_maintenance(token)`
/// at boot. At shutdown, cancel the token and await the handle: it closes
/// every database, ships the last generations and stops the loop. A bare
/// `close_all` does not stop the loop.
///
/// # Errors
///
/// [`InstallReplicationError::AlreadyReplicates`] for a second call on one
/// fleet, [`InstallReplicationError::RootInUse`] when another fleet ships
/// to the replica root, and [`InstallReplicationError::Thread`] when the
/// loop thread does not start. After `Thread` the fleet keeps a replica
/// with no loop: stop the boot.
pub fn install_replication<E: FleetEngine>(
    state: &AppState,
    fleet: &DatabaseFleet<E>,
    config: &DatabaseFleetConfig,
) -> Result<ReplicationInstall, InstallReplicationError> {
    let Some(source) = state.extension::<ReplicaSource>() else {
        if config.restore_missing {
            tracing::warn!(
                root = %fleet.root().display(),
                "restore_missing is set but [replication] is off; there is no replica to restore \
                 a missing database from"
            );
        }
        fleet.warn_if_unreplicated(config.ownership.is_some());
        return Ok(ReplicationInstall::NoReplication);
    };
    // Keep the lock to the end. Then two installs at the same time cannot
    // both claim the source.
    let mut claimed = source
        .claimed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if fleet.replication().is_some() {
        return Err(InstallReplicationError::AlreadyReplicates);
    }
    // Replica keys hold only the tenant or the slot. A second fleet would
    // write the same keys, also with the same root and another template.
    if let Some(other) = claimed.as_ref() {
        return Err(InstallReplicationError::RootInUse {
            root: source.settings.root.clone(),
            claimed_by: other.display().to_string(),
        });
    }
    if config.restore_missing && config.ownership.is_none() {
        tracing::warn!(
            root = %fleet.root().display(),
            "restore_missing is on without ownership: nothing fences the previous owner, so two \
             hosts can write one database and diverge"
        );
    }
    let replication = Arc::new(build(state, &source, config));
    fleet
        .install_replication(Arc::clone(&replication))
        .map_err(|_| InstallReplicationError::AlreadyReplicates)?;
    *claimed = Some(fleet.root().to_path_buf());
    drop(claimed);
    if let Err(e) = state.health_indicator_registry.register(
        FLEET_REPLICATION_INDICATOR,
        crate::actuator::IndicatorGroup::HealthOnly,
        replication.indicator(),
    ) {
        tracing::warn!("{e}");
    }
    replication
        .spawn_loop()
        .map_err(InstallReplicationError::Thread)?;
    tracing::info!(
        prefix = %format!("{}/fleet/", source.settings.root),
        root = %fleet.root().display(),
        engine = std::any::type_name::<E>(),
        restore_missing = config.restore_missing,
        "fleet replication is enabled"
    );
    Ok(ReplicationInstall::Installed)
}

/// Builds the replication of one fleet from the source and the section.
fn build(
    state: &AppState,
    source: &ReplicaSource,
    config: &DatabaseFleetConfig,
) -> FleetReplication {
    let restore_check_sample = if state.capabilities().restore_check {
        config.restore_check_sample
    } else {
        // The check downloads whole databases: it needs its own capability.
        0
    };
    let replication = FleetReplication::new(
        Arc::clone(&source.destination),
        source.settings.clone(),
        source.lag_alert_after,
        config.restore_missing,
        state.clock_arc(),
    )
    // The server path sets a tighter cutoff from the budget left.
    .with_final_flush(source.final_flush)
    // A deleted database's replica goes with it.
    .with_retention_after_delete(config.replica_retention_after_delete)
    .with_restore_check(source.settings.verify_interval, restore_check_sample)
    .with_entropy(state.entropy_arc())
    .with_ship_workers(config.replication_workers);
    if let Some(control) = state.pool() {
        replication.with_tombstones(Arc::new(crate::db::fleet_tombstones::FleetTombstones::new(
            control.clone(),
            state.clock_arc(),
        )))
    } else {
        tracing::warn!(
            "fleet replication has no control database: a failed purge of a deleted database's \
             replica is not retried"
        );
        replication
    }
}

/// An app state as core boots it with `[replication]` on: a file replica
/// in `scratch/replicas` and a control database at `scratch/control.db`.
#[cfg(test)]
pub async fn replicated_state(scratch: &std::path::Path) -> AppState {
    let replicas = scratch.join("replicas");
    std::fs::create_dir_all(&replicas).expect("replica directory");
    let control_url = format!("sqlite://{}", scratch.join("control.db").display());
    let control = crate::db::create_pool(&crate::config::DatabaseConfig {
        url: Some(control_url.clone()),
        ..Default::default()
    })
    .expect("control pool")
    .expect("a control url");
    let config = crate::config::ReplicationConfig {
        enabled: true,
        path: Some(replicas.display().to_string()),
        ..crate::config::ReplicationConfig::default()
    };
    let runtime = crate::time::spawn_blocking(move || {
        crate::replication::build(
            &config,
            &control_url,
            "dev",
            None,
            Arc::new(crate::time::SystemClock),
        )
    })
    .await
    .expect("the build thread")
    .expect("a file destination builds");
    let state = AppState::for_test().with_pool(control);
    ReplicaSource::from_runtime(&runtime, Duration::from_secs(30), Duration::from_secs(5))
        .install(&state);
    state
}

#[cfg(test)]
mod tests;
