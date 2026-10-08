//! Tenant isolation for the `local` job runtime (issue #3072).
//!
//! - [`FairBucket`]: one queue's jobs, served round-robin by tenant. A quiet
//!   tenant waits for one job of each other tenant, not for a full backlog.
//! - [`TenantJobIsolation`]: the per-tenant slot cap and the shuffle-shard
//!   lanes from `[jobs.tenants]`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::bulkhead::{TenantBulkhead, TenantPermit, shuffle_shard};

/// The jobs of one queue, served round-robin by tenant.
///
/// `None` is the key for jobs without a tenant.
pub(crate) struct FairBucket<T> {
    /// Tenants with jobs, in serve order.
    ring: VecDeque<Option<String>>,
    jobs: HashMap<Option<String>, VecDeque<T>>,
    len: usize,
}

impl<T> FairBucket<T> {
    pub(crate) fn new() -> Self {
        Self {
            ring: VecDeque::new(),
            jobs: HashMap::new(),
            len: 0,
        }
    }

    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn push(&mut self, tenant: Option<String>, item: T) {
        let queue = self.jobs.entry(tenant.clone()).or_default();
        if queue.is_empty() {
            self.ring.push_back(tenant);
        }
        queue.push_back(item);
        self.len = self.len.saturating_add(1);
    }

    /// Pop the first job of the first tenant that `admit` accepts, in ring
    /// order. A served tenant goes to the back of the ring. `admit` returns
    /// the permit that the job holds while it runs.
    pub(crate) fn pop_where<P>(
        &mut self,
        mut admit: impl FnMut(Option<&str>) -> Option<P>,
    ) -> Option<(T, P)> {
        for _ in 0..self.ring.len() {
            let tenant = self.ring.pop_front()?;
            let Some(permit) = admit(tenant.as_deref()) else {
                self.ring.push_back(tenant);
                continue;
            };
            let queue = self.jobs.get_mut(&tenant)?;
            let item = queue.pop_front()?;
            if queue.is_empty() {
                self.jobs.remove(&tenant);
            } else {
                self.ring.push_back(tenant);
            }
            self.len = self.len.saturating_sub(1);
            return Some((item, permit));
        }
        None
    }
}

/// The `[jobs.tenants]` limits.
pub(crate) struct TenantJobIsolation {
    slots: Arc<TenantBulkhead>,
    lanes: u16,
    lanes_per_tenant: u16,
}

impl TenantJobIsolation {
    /// `None` when every limit is off.
    pub(crate) fn from_config(config: &crate::config::JobTenantsConfig) -> Option<Arc<Self>> {
        if config.max_concurrent == 0 && config.lanes == 0 {
            return None;
        }
        Some(Arc::new(Self {
            slots: TenantBulkhead::new(config.max_concurrent),
            lanes: config.lanes,
            lanes_per_tenant: config.lanes_per_tenant.max(1),
        }))
    }

    /// The lane of worker `worker`, or `None` when lanes are off.
    pub(crate) fn lane_of_worker(&self, worker: usize) -> Option<u16> {
        if self.lanes == 0 {
            return None;
        }
        let lane = worker.checked_rem(usize::from(self.lanes)).unwrap_or(0);
        u16::try_from(lane).ok()
    }

    /// Admit a job of `tenant` on a worker of `lane`.
    ///
    /// - `None`: the job must wait (wrong lane, or the tenant is at its cap).
    /// - `Some(None)`: run it; it has no tenant, so it holds no permit.
    /// - `Some(Some(permit))`: run it while it holds `permit`.
    pub(crate) fn try_admit(
        &self,
        tenant: Option<&str>,
        lane: Option<u16>,
    ) -> Option<Option<TenantPermit>> {
        let Some(tenant) = tenant else {
            return Some(None);
        };
        if let Some(lane) = lane
            && !shuffle_shard(tenant, self.lanes, self.lanes_per_tenant).contains(&lane)
        {
            return None;
        }
        self.slots.try_acquire(tenant).map(Some)
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
