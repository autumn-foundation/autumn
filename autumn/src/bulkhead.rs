//! Per-tenant bulkheads and shuffle sharding (issue #3072).
//!
//! - [`TenantBulkhead`]: a cap on the work that one tenant has in flight.
//!   The tenancy middleware uses one for requests
//!   (`tenancy.max_concurrent_requests`) and one for database connections
//!   (`tenancy.max_db_connections`). The `local` job runtime uses one for
//!   job slots (`jobs.tenants.max_concurrent`).
//! - [`shuffle_shard`]: a stable set of lanes for a tenant. Two tenants share
//!   all their lanes only rarely, so one noisy tenant fills a small part of
//!   the lanes and slows few other tenants.
//!
//! See `docs/guide/cell-isolation.md`.

// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate".
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// A cap on the in-flight work of each tenant.
///
/// A tenant entry exists only while the tenant holds a permit. Thus tenant
/// ids from requests cannot grow the map without bound: its size is at most
/// the number of permits out.
pub struct TenantBulkhead {
    max_per_tenant: usize,
    in_flight: Mutex<HashMap<String, usize>>,
}

impl std::fmt::Debug for TenantBulkhead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantBulkhead")
            .field("max_per_tenant", &self.max_per_tenant)
            .field("tracked_tenants", &self.tracked_tenants())
            .finish()
    }
}

/// One unit of a tenant's in-flight work. Drop it to give the unit back.
#[must_use = "the permit goes back when it drops"]
pub struct TenantPermit {
    bulkhead: Arc<TenantBulkhead>,
    tenant: String,
}

impl std::fmt::Debug for TenantPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TenantPermit")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl TenantBulkhead {
    /// Make a bulkhead with `max_per_tenant` permits for each tenant. `0`
    /// sets no cap: every acquire succeeds, and the bulkhead still counts.
    #[must_use]
    pub fn new(max_per_tenant: usize) -> Arc<Self> {
        Arc::new(Self {
            max_per_tenant,
            in_flight: Mutex::new(HashMap::new()),
        })
    }

    /// The cap for each tenant. `0` means no cap.
    #[must_use]
    pub const fn max_per_tenant(&self) -> usize {
        self.max_per_tenant
    }

    /// Take a permit for `tenant`, or `None` when `tenant` is at its cap.
    #[must_use]
    pub fn try_acquire(self: &Arc<Self>, tenant: &str) -> Option<TenantPermit> {
        let mut map = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let held = map.get(tenant).copied().unwrap_or(0);
        if self.max_per_tenant != 0 && held >= self.max_per_tenant {
            return None;
        }
        map.insert(tenant.to_owned(), held.saturating_add(1));
        drop(map);
        Some(TenantPermit {
            bulkhead: Arc::clone(self),
            tenant: tenant.to_owned(),
        })
    }

    /// The permits that `tenant` holds now.
    #[must_use]
    pub fn in_flight(&self, tenant: &str) -> usize {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(tenant)
            .copied()
            .unwrap_or(0)
    }

    /// The number of tenants that hold a permit now.
    #[must_use]
    pub fn tracked_tenants(&self) -> usize {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    fn release(&self, tenant: &str) {
        let mut map = self
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = map.get_mut(tenant) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                map.remove(tenant);
            }
        }
    }
}

impl TenantPermit {
    /// The tenant that holds this permit.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }
}

impl Drop for TenantPermit {
    fn drop(&mut self) {
        self.bulkhead.release(&self.tenant);
    }
}

tokio::task_local! {
    /// The `tenancy.max_db_connections` bulkhead of the current request. The
    /// tenancy middleware sets it.
    pub(crate) static TENANT_DB_BULKHEAD: Arc<TenantBulkhead>;
}

/// Take a database-connection permit for the current tenant.
///
/// `Ok(None)` when no tenant or no cap is in scope.
///
/// # Errors
///
/// A `503` when the tenant holds `tenancy.max_db_connections` connections.
pub(crate) fn acquire_db_permit() -> Result<Option<TenantPermit>, crate::AutumnError> {
    let Ok(bulkhead) = TENANT_DB_BULKHEAD.try_with(Arc::clone) else {
        return Ok(None);
    };
    let Some(tenant) = crate::tenancy::CURRENT_TENANT
        .try_with(Clone::clone)
        .ok()
        .flatten()
    else {
        return Ok(None);
    };
    bulkhead.try_acquire(&tenant).map(Some).ok_or_else(|| {
        tracing::debug!(tenant = %tenant, "tenant database-connection cap reached");
        crate::AutumnError::service_unavailable_msg(
            "Too many database connections for this tenant; try again shortly.",
        )
    })
}

/// FNV-1a, 64 bit.
fn fnv1a_64(bytes: impl IntoIterator<Item = u8>) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.into_iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

/// The splitmix64 finalizer. FNV-1a alone has weak low bits, and a lane is
/// a low-bit remainder.
const fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The lanes of `key`: `min(size, lanes)` distinct lanes in `0..lanes`,
/// sorted.
///
/// The result is a permanent contract. It depends only on the arguments, so
/// every process and every version gives a tenant the same lanes. Round `i`
/// hashes `key`, a `0xff` separator and `i` (little endian) with FNV-1a,
/// mixes the hash with the splitmix64 finalizer, and takes it modulo `lanes`. A lane that is already taken is skipped.
/// After `MAX_ROUNDS` rounds, the lowest free lanes fill the rest, so the
/// loop always ends.
#[must_use]
pub fn shuffle_shard(key: &str, lanes: u16, size: u16) -> Vec<u16> {
    const MAX_ROUNDS: u32 = 1 << 20;
    let want = usize::from(size.min(lanes));
    let mut picked: Vec<u16> = Vec::with_capacity(want);
    let mut round: u32 = 0;
    while picked.len() < want && round < MAX_ROUNDS {
        let hash = mix64(fnv1a_64(
            key.bytes()
                .chain(std::iter::once(0xff))
                .chain(round.to_le_bytes()),
        ));
        // `want > 0` here, so `lanes > 0`. The remainder is below `lanes`,
        // so it fits in `u16`.
        let lane = u16::try_from(hash.checked_rem(u64::from(lanes)).unwrap_or(0)).unwrap_or(0);
        if !picked.contains(&lane) {
            picked.push(lane);
        }
        round = round.saturating_add(1);
    }
    let missing = want.saturating_sub(picked.len());
    let free: Vec<u16> = (0..lanes)
        .filter(|lane| !picked.contains(lane))
        .take(missing)
        .collect();
    picked.extend(free);
    picked.sort_unstable();
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_stop_at_the_cap_for_one_tenant_only() {
        let bulkhead = TenantBulkhead::new(2);
        let a1 = bulkhead.try_acquire("a").expect("first permit");
        let _a2 = bulkhead.try_acquire("a").expect("second permit");
        assert!(bulkhead.try_acquire("a").is_none(), "a is at its cap");
        assert!(bulkhead.try_acquire("b").is_some(), "b has its own cap");
        assert_eq!(bulkhead.in_flight("a"), 2);
        drop(a1);
        assert_eq!(bulkhead.in_flight("a"), 1);
        assert!(bulkhead.try_acquire("a").is_some(), "a released one permit");
    }

    #[test]
    fn a_tenant_with_no_permits_is_not_tracked() {
        let bulkhead = TenantBulkhead::new(1);
        let permit = bulkhead.try_acquire("a").expect("permit");
        assert_eq!(bulkhead.tracked_tenants(), 1);
        drop(permit);
        assert_eq!(bulkhead.tracked_tenants(), 0, "the entry goes at zero");
        assert_eq!(bulkhead.in_flight("a"), 0);
    }

    #[test]
    fn a_zero_cap_admits_everything() {
        let bulkhead = TenantBulkhead::new(0);
        let permits: Vec<_> = (0..100)
            .map(|_| bulkhead.try_acquire("a").expect("no cap"))
            .collect();
        assert_eq!(bulkhead.in_flight("a"), 100);
        drop(permits);
        assert_eq!(bulkhead.tracked_tenants(), 0);
    }

    #[test]
    fn shuffle_shard_gives_distinct_sorted_lanes_in_range() {
        for key in ["acme", "globex", "initech", ""] {
            let lanes = shuffle_shard(key, 8, 3);
            assert_eq!(lanes.len(), 3, "{key}");
            assert!(lanes.windows(2).all(|w| w[0] < w[1]), "{key}: {lanes:?}");
            assert!(lanes.iter().all(|&l| l < 8), "{key}: {lanes:?}");
        }
    }

    #[test]
    fn shuffle_shard_is_stable() {
        // A permanent contract: a change moves every tenant to new lanes.
        assert_eq!(shuffle_shard("acme", 8, 2), shuffle_shard("acme", 8, 2));
        assert_eq!(shuffle_shard("acme", 8, 2), vec![0, 6]);
        assert_eq!(shuffle_shard("globex", 8, 2), vec![0, 3]);
        assert_eq!(shuffle_shard("initech", 16, 3), vec![0, 10, 14]);
    }

    #[test]
    fn shuffle_shard_clamps_and_handles_zero() {
        assert!(shuffle_shard("a", 0, 2).is_empty());
        assert!(shuffle_shard("a", 4, 0).is_empty());
        assert_eq!(shuffle_shard("a", 3, 9), vec![0, 1, 2]);
    }

    #[test]
    fn few_tenants_share_all_lanes_with_a_noisy_one() {
        // 8 lanes, 2 per tenant: C(8,2) = 28 lane pairs, so about 1/28 of
        // tenants have the same pair as the noisy tenant.
        let noisy = shuffle_shard("noisy", 8, 2);
        let full_overlap = (0..1000)
            .filter(|i| shuffle_shard(&format!("tenant-{i}"), 8, 2) == noisy)
            .count();
        // 1000 / 28 is about 36.
        assert!(full_overlap < 60, "{full_overlap} of 1000 share all lanes");
    }
}
