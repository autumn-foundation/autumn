//! The request path of a fleet, for engine plugins (ADR 0040 §2, §4).
//!
//! [`resolve`] finds the fleet database of a request. [`resolve_key`] does
//! the same for a job or a task, with no request. Both return a
//! [`FleetLease`]: the engine handle, held open until the lease drops.
//!
//! Core has no generic extractor. Each engine plugin writes its own, with its
//! own name and error mapping:
//!
//! ```rust,ignore
//! pub struct FleetDuckDb(FleetLease<DuckDbEngine>);
//!
//! impl FromRequestParts<AppState> for FleetDuckDb {
//!     type Rejection = AutumnError;
//!
//!     async fn from_request_parts(
//!         parts: &mut Parts,
//!         state: &AppState,
//!     ) -> Result<Self, AutumnError> {
//!         autumn_web::fleet::resolve::<DuckDbEngine>(parts, state)
//!             .await
//!             .map(Self)
//!             .map_err(AutumnError::from)
//!     }
//! }
//! ```
//!
//! # Where the fleet comes from
//!
//! `resolve::<E>` uses the [`DatabaseFleet<E>`] in the app state. A plugin
//! puts its fleet there with [`AppState::insert_extension`]. With `sqlite`,
//! `resolve::<SqliteEngine>` also finds the fleet of `[database.fleet]`.
//!
//! [`DatabaseFleet<E>`]: crate::db::fleet::DatabaseFleet
//! [`AppState::insert_extension`]: crate::AppState::insert_extension

use std::sync::Arc;

use crate::AppState;
use crate::db::fleet::{DatabaseFleet, FleetDatabase, FleetError};
use crate::db::fleet_engine::FleetEngine;
use crate::db::fleet_ownership::SlotOwnershipStore;
use crate::fleet_layout::FleetDbKey;
use crate::lock::FencingToken;
use crate::sharding::{ShardKey, ShardLease};

/// One open fleet database, held for one request or one task.
///
/// While a lease (or a clone of it) lives, the fleet does not close the
/// database. Do not keep a lease in a long-lived cache: each one keeps a
/// database open.
pub struct FleetLease<E: FleetEngine> {
    db: FleetDatabase<E>,
}

impl<E: FleetEngine> FleetLease<E> {
    /// The engine handle.
    #[must_use]
    pub const fn handle(&self) -> &E::Handle {
        self.db.handle()
    }

    /// Which database this is.
    #[must_use]
    pub const fn key(&self) -> &FleetDbKey {
        self.db.key()
    }

    /// The ownership epoch the database opened under. `None` without
    /// ownership.
    #[must_use]
    pub const fn epoch(&self) -> Option<FencingToken> {
        self.db.epoch()
    }

    /// A lease to give to work that outlives this value.
    #[must_use]
    pub fn lease(&self) -> ShardLease {
        self.db.lease()
    }

    /// The fleet database.
    #[must_use]
    pub const fn database(&self) -> &FleetDatabase<E> {
        &self.db
    }

    /// The fleet database, which keeps the lease.
    #[must_use]
    pub fn into_database(self) -> FleetDatabase<E> {
        self.db
    }
}

impl<E: FleetEngine> Clone for FleetLease<E> {
    fn clone(&self) -> Self {
        Self {
            db: self.db.clone(),
        }
    }
}

impl<E: FleetEngine> std::fmt::Debug for FleetLease<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FleetLease")
            .field("key", self.db.key())
            .field("epoch", &self.db.epoch())
            .finish_non_exhaustive()
    }
}

impl<E: FleetEngine> std::ops::Deref for FleetLease<E> {
    type Target = E::Handle;

    fn deref(&self) -> &E::Handle {
        self.db.handle()
    }
}

/// Opens the fleet database of a request and leases it.
///
/// It finds the routing key of the request (a
/// [`ShardKeyOverride`](crate::sharding::ShardKeyOverride), then the
/// tenant), makes the database key, checks the owner and opens the database.
/// If the fleet creates on demand, the open makes a missing database.
///
/// # Errors
///
/// - [`FleetError::NoFleet`] (`503`): the app has no fleet of engine `E`.
/// - [`FleetError::NoRoutingKey`] (`400`): the request names no tenant.
/// - [`FleetError::Unavailable`] (`503`): the app state has no config.
/// - [`FleetError::NotOwner`] (`421`): this host does not own the slot.
///   Nothing is opened or made.
/// - Any other error of [`DatabaseFleet::open`].
pub async fn resolve<E: FleetEngine>(
    parts: &mut http::request::Parts,
    state: &AppState,
) -> Result<FleetLease<E>, FleetError> {
    let fleet = installed::<E>(state)?;
    let routing = crate::sharding::request_routing_key(parts, state)
        .await
        .map_err(|error| match error {
            // A fault of the app, not of the request.
            crate::sharding::RoutingKeyError::NoConfig => FleetError::Unavailable {
                name: "unknown".to_owned(),
                detail: "the app state has no config".to_owned(),
            },
            crate::sharding::RoutingKeyError::Tenant(error) => FleetError::NoRoutingKey {
                detail: format!(
                    "{error}. Enable [tenancy], or insert a ShardKeyOverride request \
                     extension from middleware"
                ),
            },
        })?;
    open_leased(&fleet, ShardKey::Str(&routing)).await
}

/// Opens the fleet database of `key` and leases it, with no request. Use it
/// in jobs and tasks. `key` is a tenant id, or another
/// [`ShardKey`](crate::sharding::ShardKey).
///
/// # Errors
///
/// As [`resolve`], but never [`FleetError::NoRoutingKey`].
pub async fn resolve_key<'k, E: FleetEngine>(
    state: &AppState,
    key: impl Into<ShardKey<'k>>,
) -> Result<FleetLease<E>, FleetError> {
    let fleet = installed::<E>(state)?;
    open_leased(&fleet, key.into()).await
}

/// Makes the key, checks the owner and opens the database.
async fn open_leased<E: FleetEngine>(
    fleet: &DatabaseFleet<E>,
    key: ShardKey<'_>,
) -> Result<FleetLease<E>, FleetError> {
    let key = fleet.key_for_shard_key(key)?;
    // The open checks the owner first, before it touches the disk.
    Ok(FleetLease {
        db: fleet.open(&key).await?,
    })
}

fn installed<E: FleetEngine>(state: &AppState) -> Result<DatabaseFleet<E>, FleetError> {
    fleet_of::<E>(state).ok_or_else(|| FleetError::NoFleet {
        engine: std::any::type_name::<E>(),
    })
}

/// The fleet of engine `E` in `state`: the [`DatabaseFleet<E>`] extension.
/// With `sqlite`, for `E = SqliteEngine`, also the fleet of
/// `[database.fleet]`.
#[must_use]
pub fn fleet_of<E: FleetEngine>(state: &AppState) -> Option<DatabaseFleet<E>> {
    if let Some(fleet) = state.extension::<DatabaseFleet<E>>() {
        return Some((*fleet).clone());
    }
    #[cfg(feature = "sqlite")]
    if let Some(fleet) = state.shards().and_then(crate::sharding::ShardSet::fleet) {
        let fleet: &dyn std::any::Any = fleet;
        return fleet.downcast_ref::<DatabaseFleet<E>>().cloned();
    }
    None
}

/// The store of `AppBuilder::fleet_ownership_store`.
struct InstalledOwnershipStore(Arc<dyn SlotOwnershipStore>);

/// Installs `store` as the ownership store of the app's fleets.
pub(crate) fn install_ownership_store(state: &AppState, store: Arc<dyn SlotOwnershipStore>) {
    state.insert_extension(InstalledOwnershipStore(store));
}

/// The ownership store of the app's fleets.
///
/// It is the store of [`AppBuilder::fleet_ownership_store`], else the SQL
/// store of the control database (`SqlSlotOwnership` with `sqlite`,
/// `PgSlotOwnership` with Postgres). `None` with neither.
///
/// Every host of a fleet must use the same store. One store holds the
/// ownership of one fleet: two fleets on one store share their rows, so the
/// release of one fleet ends the lease of the other.
///
/// [`AppBuilder::fleet_ownership_store`]: crate::app::AppBuilder::fleet_ownership_store
#[must_use]
pub fn ownership_store(state: &AppState) -> Option<Arc<dyn SlotOwnershipStore>> {
    if let Some(installed) = state.extension::<InstalledOwnershipStore>() {
        return Some(Arc::clone(&installed.0));
    }
    control_store(state)
}

/// The SQL store on the control database.
#[cfg(feature = "sqlite")]
fn control_store(state: &AppState) -> Option<Arc<dyn SlotOwnershipStore>> {
    let pool = state.pool()?.clone();
    Some(Arc::new(crate::db::fleet_ownership::SqlSlotOwnership::new(
        pool,
        state.clock_arc(),
    )))
}

/// The SQL store on the control database.
#[cfg(all(feature = "postgres", not(feature = "sqlite")))]
fn control_store(state: &AppState) -> Option<Arc<dyn SlotOwnershipStore>> {
    let pool = state.pool()?.clone();
    Some(Arc::new(crate::db::fleet_ownership::PgSlotOwnership::new(
        pool,
        state.clock_arc(),
    )))
}

/// No SQL backend, no store.
#[cfg(not(any(feature = "sqlite", feature = "postgres")))]
const fn control_store(_: &AppState) -> Option<Arc<dyn SlotOwnershipStore>> {
    None
}

#[cfg(test)]
mod tests;
