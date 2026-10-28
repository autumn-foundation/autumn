//! Health check result cache with a short TTL and single-flight refresh.
//!
//! Many probers can call `/ready` at the same time. Without a cache, each
//! call runs each check again. This adds load to a dependency that already
//! has a problem. This cache keeps one result for a short TTL. When the
//! result is stale, only one refresh runs. The other callers wait for it.
//!
//! The refresh runs in its own task. A caller that is cancelled (for
//! example, the prober closes the connection) does not stop the refresh.
//! The next caller gets its result.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::FutureExt as _;
use futures::future::{BoxFuture, Shared};
use tokio::time::Instant;

/// Default time to keep a health check result.
#[cfg(feature = "db")]
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(1);

/// Default time limit for one built-in dependency ping.
#[cfg(any(feature = "db", feature = "redis"))]
pub const DEFAULT_PING_TIMEOUT: Duration = Duration::from_secs(2);

/// The refresh that callers share. `None` when the refresh task failed.
type Refresh<T> = Shared<BoxFuture<'static, Option<T>>>;

struct Slot<T> {
    value: Option<(Instant, T)>,
    refresh: Option<Refresh<T>>,
}

/// Caches one value for a TTL. Refreshes are single-flight.
pub struct SingleFlightCache<T> {
    ttl_ms: AtomicU64,
    slot: Arc<Mutex<Slot<T>>>,
}

impl<T: Clone + Send + Sync + 'static> SingleFlightCache<T> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl_ms: AtomicU64::new(duration_ms(ttl)),
            slot: Arc::new(Mutex::new(Slot {
                value: None,
                refresh: None,
            })),
        }
    }

    /// Set the TTL. A zero TTL turns the cache off.
    pub fn set_ttl(&self, ttl: Duration) {
        self.ttl_ms.store(duration_ms(ttl), Ordering::Relaxed);
    }

    /// Return the cached value, or refresh it and cache the result.
    ///
    /// When a refresh already runs, wait for it. Do not start a second one.
    /// This is also true when the TTL is zero: concurrent callers share one
    /// refresh, and a later caller starts a new one. When the refresh task
    /// fails (it panics), return `on_failure()` and do not cache it.
    pub async fn get_or_refresh<F, Fut>(&self, refresh: F, on_failure: impl FnOnce() -> T) -> T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let ttl = Duration::from_millis(self.ttl_ms.load(Ordering::Relaxed));
        let shared = {
            let mut slot = lock(&self.slot);
            if let Some((at, value)) = &slot.value
                && at.elapsed() < ttl
            {
                return value.clone();
            }
            if let Some(shared) = &slot.refresh {
                shared.clone()
            } else {
                let shared = self.start_refresh(refresh());
                slot.refresh = Some(shared.clone());
                shared
            }
        };
        shared.await.unwrap_or_else(on_failure)
    }

    /// Spawn the refresh task. It stores its result, so the result is cached
    /// also when every caller is cancelled. Call with the slot lock held: the
    /// task needs that lock, so it cannot finish before the caller records it.
    fn start_refresh<Fut>(&self, refresh: Fut) -> Refresh<T>
    where
        Fut: Future<Output = T> + Send + 'static,
    {
        let slot = Arc::clone(&self.slot);
        let task = tokio::spawn(async move {
            let value = refresh.await;
            let mut slot = lock(&slot);
            slot.value = Some((Instant::now(), value.clone()));
            slot.refresh = None;
            drop(slot);
            value
        });
        let slot = Arc::clone(&self.slot);
        let joined: BoxFuture<'static, Option<T>> = Box::pin(async move {
            let value = task.await.ok();
            if value.is_none() {
                // The task failed and did not clear the refresh. Clear it, so
                // the next caller starts a new refresh.
                lock(&slot).refresh = None;
            }
            value
        });
        joined.shared()
    }
}

/// Result of [`race_kept_connection`].
#[cfg(any(feature = "db", feature = "redis"))]
pub enum Raced<C, E> {
    /// The kept connection answered.
    Kept,
    /// A new connection answered first, or the kept one failed.
    Fresh(C),
    /// Both failed. The error is from the new connection.
    Failed(E),
}

/// Ping on a kept connection. If it does not answer in half the budget,
/// also open a new connection, and use the first one that answers.
///
/// A kept connection that hangs (the server moved, the network dropped it)
/// must leave time for a new one. A kept connection that is only slow must
/// not be dropped while it can still answer in the budget.
#[cfg(any(feature = "db", feature = "redis"))]
pub async fn race_kept_connection<C, E, Fresh>(
    kept: impl Future<Output = bool>,
    fresh: impl FnOnce() -> Fresh,
    budget: Duration,
) -> Raced<C, E>
where
    Fresh: Future<Output = Result<C, E>>,
{
    let mut kept = std::pin::pin!(kept);
    match tokio::time::timeout(budget / 2, &mut kept).await {
        Ok(true) => return Raced::Kept,
        Ok(false) => {
            return match fresh().await {
                Ok(conn) => Raced::Fresh(conn),
                Err(error) => Raced::Failed(error),
            };
        }
        Err(_slow) => {}
    }
    let mut fresh = std::pin::pin!(fresh());
    tokio::select! {
        answered = &mut kept => {
            if answered {
                Raced::Kept
            } else {
                match fresh.await {
                    Ok(conn) => Raced::Fresh(conn),
                    Err(error) => Raced::Failed(error),
                }
            }
        }
        opened = &mut fresh => match opened {
            Ok(conn) => Raced::Fresh(conn),
            Err(error) => {
                if kept.await {
                    Raced::Kept
                } else {
                    Raced::Failed(error)
                }
            }
        },
    }
}

fn lock<T>(slot: &Mutex<T>) -> MutexGuard<'_, T> {
    slot.lock().unwrap_or_else(PoisonError::into_inner)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A refresh that waits `delay`, then returns how many refreshes ran.
    fn counted(
        calls: &Arc<AtomicUsize>,
        delay: Duration,
    ) -> impl FnOnce() -> BoxFuture<'static, usize> + use<> {
        let calls = Arc::clone(calls);
        move || {
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                calls.fetch_add(1, Ordering::SeqCst) + 1
            })
        }
    }

    const FAILED: usize = usize::MAX;

    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_share_one_refresh() {
        let cache = Arc::new(SingleFlightCache::new(Duration::from_secs(1)));
        let calls = Arc::new(AtomicUsize::new(0));

        let tasks: Vec<_> = (0..64)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let refresh = counted(&calls, Duration::from_millis(50));
                tokio::spawn(async move { cache.get_or_refresh(refresh, || FAILED).await })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap(), 1);
        }

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_value_is_refreshed_after_ttl() {
        let cache = SingleFlightCache::new(Duration::from_secs(1));
        let calls = Arc::new(AtomicUsize::new(0));
        let get = || cache.get_or_refresh(counted(&calls, Duration::ZERO), || FAILED);

        assert_eq!(get().await, 1);
        tokio::time::advance(Duration::from_millis(999)).await;
        assert_eq!(get().await, 1);
        tokio::time::advance(Duration::from_millis(2)).await;
        assert_eq!(get().await, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn zero_ttl_runs_every_call() {
        let cache = SingleFlightCache::new(Duration::ZERO);
        let calls = Arc::new(AtomicUsize::new(0));

        for expected in 1..=3 {
            let value = cache
                .get_or_refresh(counted(&calls, Duration::ZERO), || FAILED)
                .await;
            assert_eq!(value, expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn zero_ttl_still_shares_a_running_refresh() {
        let cache = Arc::new(SingleFlightCache::new(Duration::ZERO));
        let calls = Arc::new(AtomicUsize::new(0));

        let tasks: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let refresh = counted(&calls, Duration::from_millis(50));
                tokio::spawn(async move { cache.get_or_refresh(refresh, || FAILED).await })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap(), 1);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    async fn answer_after(delay: Duration, ok: bool) -> bool {
        tokio::time::sleep(delay).await;
        ok
    }

    async fn open_after(delay: Duration, ok: bool) -> Result<&'static str, &'static str> {
        tokio::time::sleep(delay).await;
        if ok { Ok("new") } else { Err("refused") }
    }

    #[tokio::test(start_paused = true)]
    async fn slow_kept_connection_inside_the_budget_is_kept() {
        // The kept connection answers at 1.5 s of 2 s. A new one needs 1 s
        // more, so it cannot answer in the budget.
        let raced = race_kept_connection(
            answer_after(Duration::from_millis(1_500), true),
            || open_after(Duration::from_secs(1), true),
            Duration::from_secs(2),
        )
        .await;
        assert!(matches!(raced, Raced::Kept));
    }

    #[tokio::test(start_paused = true)]
    async fn hung_kept_connection_is_replaced() {
        let raced = race_kept_connection(
            std::future::pending::<bool>(),
            || open_after(Duration::from_millis(200), true),
            Duration::from_secs(2),
        )
        .await;
        assert!(matches!(raced, Raced::Fresh("new")));
    }

    #[tokio::test(start_paused = true)]
    async fn failed_kept_connection_opens_a_new_one_at_once() {
        let started = Instant::now();
        let raced = race_kept_connection(
            answer_after(Duration::ZERO, false),
            || open_after(Duration::ZERO, false),
            Duration::from_secs(2),
        )
        .await;
        assert!(matches!(raced, Raced::Failed("refused")));
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn set_ttl_applies_to_next_call() {
        let cache = SingleFlightCache::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));

        let first = cache
            .get_or_refresh(counted(&calls, Duration::ZERO), || FAILED)
            .await;
        assert_eq!(first, 1);
        cache.set_ttl(Duration::ZERO);
        let second = cache
            .get_or_refresh(counted(&calls, Duration::ZERO), || FAILED)
            .await;
        assert_eq!(second, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_caller_does_not_stop_the_refresh() {
        let cache = Arc::new(SingleFlightCache::new(Duration::from_secs(1)));
        let calls = Arc::new(AtomicUsize::new(0));

        let cancelled = {
            let cache = Arc::clone(&cache);
            let refresh = counted(&calls, Duration::from_secs(10));
            tokio::spawn(async move { cache.get_or_refresh(refresh, || FAILED).await })
        };
        tokio::time::advance(Duration::from_millis(10)).await;
        cancelled.abort();
        let _ = cancelled.await;

        // This caller joins the first refresh. It does not start a second one.
        let value = cache
            .get_or_refresh(counted(&calls, Duration::ZERO), || FAILED)
            .await;
        assert_eq!(value, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn result_is_cached_when_every_caller_is_cancelled() {
        let cache = Arc::new(SingleFlightCache::new(Duration::from_secs(60)));
        let calls = Arc::new(AtomicUsize::new(0));

        let cancelled = {
            let cache = Arc::clone(&cache);
            let refresh = counted(&calls, Duration::from_millis(100));
            tokio::spawn(async move { cache.get_or_refresh(refresh, || FAILED).await })
        };
        tokio::time::advance(Duration::from_millis(10)).await;
        cancelled.abort();
        let _ = cancelled.await;
        tokio::time::sleep(Duration::from_millis(200)).await;

        let value = cache
            .get_or_refresh(counted(&calls, Duration::ZERO), || FAILED)
            .await;
        assert_eq!(value, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_refresh_is_not_cached() {
        let cache = SingleFlightCache::new(Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));

        let failed = cache
            .get_or_refresh(|| async { panic!("refresh failed") }, || FAILED)
            .await;
        assert_eq!(failed, FAILED);

        let value = cache
            .get_or_refresh(counted(&calls, Duration::ZERO), || FAILED)
            .await;
        assert_eq!(value, 1);
    }
}
