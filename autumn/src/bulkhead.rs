//! Per-tenant bulkheads and shuffle sharding (issue #3072).
//!
//! - [`TenantBulkhead`]: a cap on the work that one tenant has in flight.
//! - [`shuffle_shard`]: a stable set of lanes for a tenant.

use std::sync::Arc;

/// A cap on the in-flight work of each tenant.
pub struct TenantBulkhead;

/// One unit of a tenant's in-flight work.
pub struct TenantPermit;

impl TenantBulkhead {
    /// Make a bulkhead with `max_per_tenant` permits for each tenant.
    #[must_use]
    pub fn new(_max_per_tenant: usize) -> Arc<Self> {
        todo!()
    }

    /// Take a permit for `tenant`.
    #[must_use]
    pub fn try_acquire(self: &Arc<Self>, _tenant: &str) -> Option<TenantPermit> {
        todo!()
    }

    /// The permits that `tenant` holds now.
    #[must_use]
    pub fn in_flight(&self, _tenant: &str) -> usize {
        todo!()
    }

    /// The number of tenants that hold a permit now.
    #[must_use]
    pub fn tracked_tenants(&self) -> usize {
        todo!()
    }
}

/// The lanes of `key`.
#[must_use]
pub fn shuffle_shard(_key: &str, _lanes: u16, _size: u16) -> Vec<u16> {
    todo!()
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
        assert_eq!(shuffle_shard("acme", 8, 2), vec![2, 7]);
        assert_eq!(shuffle_shard("globex", 8, 2), vec![1, 6]);
        assert_eq!(shuffle_shard("initech", 16, 3), vec![3, 9, 14]);
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
        assert!(full_overlap < 100, "{full_overlap} of 1000 share all lanes");
    }
}
