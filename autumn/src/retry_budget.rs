//! Retry budget for outbound HTTP (issue #3058).
//!
//! A token bucket for each upstream host. It stops retry storms: when an
//! upstream fails, retries at each layer multiply the load on it.
//!
//! - The bucket starts full, with `capacity` tokens.
//! - A retry after a `429` costs `throttling_cost` tokens. A retry after a
//!   `5xx`, a connect error or a timeout costs `transient_cost` tokens.
//! - Each first attempt adds `retry_ratio x transient_cost` tokens. Thus, when
//!   the bucket is empty, about `retry_ratio` of requests can retry.
//! - A retry that succeeds gives its tokens back.
//! - A first attempt never waits for tokens.
//!
//! The defaults (500, 14, 5) are the AWS SDK values. A `retry_ratio` of `0.1`
//! is the Google SRE limit of 10 %.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::config::RetryBudgetConfig;

/// Tokens are kept in thousandths, so a fractional refill is exact.
const SCALE: u64 = 1_000;

/// The maximum number of hosts with their own bucket. More hosts share one
/// overflow bucket, so a client that calls many hosts uses bounded memory.
pub(crate) const MAX_HOSTS: usize = 1_024;

/// Why a retry is necessary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryKind {
    /// The upstream sent `429 Too Many Requests`.
    Throttling,
    /// A `5xx`, a connect error or a timeout.
    Transient,
}

/// The token bucket for one host. See the [module docs](self).
#[derive(Debug)]
pub struct RetryBudget {
    milli_tokens: AtomicU64,
    capacity: u64,
    transient_cost: u64,
    throttling_cost: u64,
    refill: u64,
}

impl RetryBudget {
    /// A full bucket with the settings in `config`.
    #[must_use]
    pub fn new(config: &RetryBudgetConfig) -> Self {
        let capacity = u64::from(config.capacity).saturating_mul(SCALE);
        let transient_cost = u64::from(config.transient_cost).saturating_mul(SCALE);
        // Float to integer casts saturate, and a NaN becomes 0.
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "the ratio is clamped to [0, 1]; the cast saturates"
        )]
        let refill = (config.retry_ratio.clamp(0.0, 1.0) * transient_cost as f64).round() as u64;
        Self {
            milli_tokens: AtomicU64::new(capacity),
            capacity,
            transient_cost,
            throttling_cost: u64::from(config.throttling_cost).saturating_mul(SCALE),
            refill,
        }
    }

    /// Record a first attempt: add the refill, up to the capacity.
    pub fn record_request(&self) {
        self.add(self.refill);
    }

    /// Take the tokens for one retry of `kind`. `false` when the bucket has
    /// too few tokens; then do not retry.
    #[must_use]
    pub fn try_acquire(&self, kind: RetryKind) -> bool {
        let cost = self.cost(kind);
        self.milli_tokens
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |tokens| {
                tokens.checked_sub(cost)
            })
            .is_ok()
    }

    /// Give back the tokens of a retry of `kind` that succeeded.
    pub fn release(&self, kind: RetryKind) {
        self.add(self.cost(kind));
    }

    /// The tokens in the bucket now.
    #[must_use]
    pub fn available(&self) -> f64 {
        #[allow(clippy::cast_precision_loss, reason = "a display value")]
        let tokens = self.milli_tokens.load(Ordering::Acquire) as f64;
        tokens / SCALE as f64
    }

    const fn cost(&self, kind: RetryKind) -> u64 {
        match kind {
            RetryKind::Throttling => self.throttling_cost,
            RetryKind::Transient => self.transient_cost,
        }
    }

    fn add(&self, amount: u64) {
        let _ = self
            .milli_tokens
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |tokens| {
                Some(tokens.saturating_add(amount).min(self.capacity))
            });
    }
}

/// The buckets of one client or app, one for each host.
#[derive(Debug)]
pub struct RetryBudgets {
    config: RetryBudgetConfig,
    hosts: Mutex<HashMap<String, Arc<RetryBudget>>>,
    overflow: Arc<RetryBudget>,
}

impl RetryBudgets {
    /// An empty set of buckets with the settings in `config`.
    #[must_use]
    pub fn new(config: &RetryBudgetConfig) -> Self {
        Self {
            config: config.clone(),
            hosts: Mutex::new(HashMap::new()),
            overflow: Arc::new(RetryBudget::new(config)),
        }
    }

    /// The bucket for `host`. Host names match whatever their case.
    #[must_use]
    pub fn for_host(&self, host: &str) -> Arc<RetryBudget> {
        let key = host.to_ascii_lowercase();
        let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(budget) = hosts.get(&key) {
            return Arc::clone(budget);
        }
        if hosts.len() >= MAX_HOSTS {
            return Arc::clone(&self.overflow);
        }
        let budget = Arc::new(RetryBudget::new(&self.config));
        hosts.insert(key, Arc::clone(&budget));
        budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RetryBudgetConfig {
        RetryBudgetConfig::default()
    }

    fn drain(budget: &RetryBudget, kind: RetryKind) -> u32 {
        let mut count = 0;
        while budget.try_acquire(kind) {
            count += 1;
        }
        count
    }

    #[test]
    fn a_full_bucket_allows_capacity_over_cost_retries() {
        let budget = RetryBudget::new(&config());
        assert_eq!(drain(&budget, RetryKind::Transient), 500 / 14);
        let budget = RetryBudget::new(&config());
        assert_eq!(drain(&budget, RetryKind::Throttling), 500 / 5);
    }

    #[test]
    fn an_empty_bucket_refills_at_the_retry_ratio() {
        let budget = RetryBudget::new(&config());
        drain(&budget, RetryKind::Transient);
        let mut retries = 0;
        for _ in 0..1_000 {
            budget.record_request();
            if budget.try_acquire(RetryKind::Transient) {
                retries += 1;
            }
        }
        assert!((95..=105).contains(&retries), "about 10 %: {retries}");
    }

    #[test]
    fn the_bucket_never_goes_above_capacity() {
        let budget = RetryBudget::new(&config());
        for _ in 0..10_000 {
            budget.record_request();
        }
        assert!((budget.available() - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_successful_retry_gives_its_tokens_back() {
        let budget = RetryBudget::new(&config());
        assert!(budget.try_acquire(RetryKind::Transient));
        assert!((budget.available() - 486.0).abs() < f64::EPSILON);
        budget.release(RetryKind::Transient);
        assert!((budget.available() - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn hosts_have_their_own_bucket() {
        let budgets = RetryBudgets::new(&config());
        let a = budgets.for_host("a.example");
        assert!(Arc::ptr_eq(&a, &budgets.for_host("A.Example")));
        drain(&a, RetryKind::Transient);
        assert!(
            budgets
                .for_host("b.example")
                .try_acquire(RetryKind::Transient)
        );
    }

    #[test]
    fn hosts_past_the_limit_share_one_bucket() {
        let budgets = RetryBudgets::new(&config());
        for index in 0..MAX_HOSTS {
            let _ = budgets.for_host(&format!("h{index}"));
        }
        let first = budgets.for_host("late-1");
        let second = budgets.for_host("late-2");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(budgets.hosts.lock().unwrap().len(), MAX_HOSTS);
    }
}
