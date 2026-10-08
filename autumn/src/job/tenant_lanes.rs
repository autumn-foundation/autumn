//! Tenant isolation for the `local` job runtime (issue #3072).
//!
//! - [`FairBucket`]: one queue's jobs, served round-robin by tenant.
//! - [`TenantJobIsolation`]: the per-tenant slot cap and the shuffle-shard
//!   lanes from `[jobs.tenants]`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::bulkhead::{TenantBulkhead, TenantPermit};

/// The jobs of one queue, served round-robin by tenant.
pub(crate) struct FairBucket<T> {
    _items: Vec<(Option<String>, T)>,
}

impl<T> FairBucket<T> {
    pub(crate) fn new() -> Self {
        todo!()
    }

    pub(crate) fn len(&self) -> usize {
        todo!()
    }

    pub(crate) fn push(&mut self, _tenant: Option<String>, _item: T) {
        todo!()
    }

    pub(crate) fn pop_where<P>(
        &mut self,
        _admit: impl FnMut(Option<&str>) -> Option<P>,
    ) -> Option<(T, P)> {
        todo!()
    }
}

/// The `[jobs.tenants]` limits.
pub(crate) struct TenantJobIsolation {
    _bulkhead: Arc<TenantBulkhead>,
}

impl TenantJobIsolation {
    pub(crate) fn from_config(_config: &crate::config::JobTenantsConfig) -> Option<Arc<Self>> {
        todo!()
    }

    pub(crate) fn lane_of_worker(&self, _worker: usize) -> Option<u16> {
        todo!()
    }

    pub(crate) fn try_admit(
        &self,
        _tenant: Option<&str>,
        _lane: Option<u16>,
    ) -> Option<Option<TenantPermit>> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::JobTenantsConfig;

    fn take_all(bucket: &mut FairBucket<&'static str>) -> Vec<&'static str> {
        let mut out = Vec::new();
        while let Some((item, ())) = bucket.pop_where(|_| Some(())) {
            out.push(item);
        }
        out
    }

    #[test]
    fn fair_bucket_serves_tenants_round_robin() {
        let mut bucket = FairBucket::new();
        for item in ["n1", "n2", "n3", "n4"] {
            bucket.push(Some("noisy".into()), item);
        }
        bucket.push(Some("quiet".into()), "q1");
        bucket.push(None, "s1");
        assert_eq!(bucket.len(), 6);
        // The quiet tenant waits for one noisy job, not for four.
        assert_eq!(take_all(&mut bucket), ["n1", "q1", "s1", "n2", "n3", "n4"]);
        assert_eq!(bucket.len(), 0);
    }

    #[test]
    fn fair_bucket_skips_a_tenant_that_is_not_admitted() {
        let mut bucket = FairBucket::new();
        bucket.push(Some("noisy".into()), "n1");
        bucket.push(Some("quiet".into()), "q1");
        let popped = bucket.pop_where(|t| (t != Some("noisy")).then_some(()));
        assert_eq!(popped.map(|(item, ())| item), Some("q1"));
        assert!(bucket.pop_where(|t| (t != Some("noisy")).then_some(())).is_none());
        assert_eq!(bucket.len(), 1, "the skipped job stays");
    }

    fn config(max_concurrent: usize, lanes: u16, lanes_per_tenant: u16) -> JobTenantsConfig {
        JobTenantsConfig {
            max_concurrent,
            lanes,
            lanes_per_tenant,
        }
    }

    #[test]
    fn isolation_is_off_by_default() {
        assert!(TenantJobIsolation::from_config(&JobTenantsConfig::default()).is_none());
    }

    #[test]
    fn the_cap_applies_per_tenant_and_not_to_untenanted_jobs() {
        let isolation = TenantJobIsolation::from_config(&config(1, 0, 0)).expect("on");
        assert_eq!(isolation.lane_of_worker(3), None, "no lanes");
        let held = isolation.try_admit(Some("a"), None).expect("first slot");
        assert!(held.is_some(), "a tenant job holds a permit");
        assert!(isolation.try_admit(Some("a"), None).is_none(), "a is at its cap");
        assert!(isolation.try_admit(Some("b"), None).is_some(), "b has its own cap");
        assert!(
            matches!(isolation.try_admit(None, None), Some(None)),
            "untenanted jobs are not capped"
        );
        drop(held);
        assert!(isolation.try_admit(Some("a"), None).is_some());
    }

    #[test]
    fn a_worker_runs_only_tenants_whose_shard_holds_its_lane() {
        let isolation = TenantJobIsolation::from_config(&config(0, 4, 1)).expect("on");
        let lane = crate::bulkhead::shuffle_shard("a", 4, 1)[0];
        let workers: Vec<usize> = (0..8)
            .filter(|&w| isolation.lane_of_worker(w) == Some(lane))
            .collect();
        assert_eq!(workers.len(), 2, "8 workers on 4 lanes: 2 per lane");
        for worker in 0..8 {
            let admitted = isolation
                .try_admit(Some("a"), isolation.lane_of_worker(worker))
                .is_some();
            assert_eq!(admitted, workers.contains(&worker), "worker {worker}");
        }
        assert!(
            isolation.try_admit(None, Some((lane + 1) % 4)).is_some(),
            "untenanted jobs run on every lane"
        );
    }
}
