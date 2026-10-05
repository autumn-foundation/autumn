//! Readiness ping for a database server: `SELECT 1` with a time limit and a
//! cached, single-flight result (issue #3059).
//!
//! The ping uses one dedicated connection. The pool manager makes it, so it
//! gets the same TLS and connection setup as the pool. It is not in the pool,
//! so a fully checked-out pool cannot block the ping. Pool saturation is load,
//! not failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;

use crate::health_cache::SingleFlightCache;

pub type DbPool = diesel_async::pooled_connection::deadpool::Pool<crate::db::RuntimeConnection>;

/// Result of one ping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbPingStatus {
    /// `true` when the server answered `SELECT 1` in time.
    pub up: bool,
    /// The failure reason. `None` when `up` is `true`.
    pub error: Option<String>,
}

/// Sends one `SELECT 1`. Tests replace it with a fake.
pub trait DbPing: Send + Sync + 'static {
    /// Ping the server of `pool`. `budget` is the time limit of the caller.
    fn ping(
        self: Arc<Self>,
        pool: DbPool,
        budget: Duration,
    ) -> BoxFuture<'static, Result<(), String>>;
}

/// Pings on one kept connection that is not in the pool.
#[derive(Default)]
struct DedicatedConnectionPing {
    conn: tokio::sync::Mutex<Option<crate::db::RuntimeConnection>>,
}

impl DbPing for DedicatedConnectionPing {
    fn ping(
        self: Arc<Self>,
        pool: DbPool,
        budget: Duration,
    ) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(async move {
            let Ok(mut slot) = self.conn.try_lock() else {
                // Another ping uses the kept connection (the cache is off).
                // Do not wait for it: use a temporary connection.
                return new_connection_ping(&pool).await.map(drop);
            };
            // Take the connection out first. If this future is cancelled, a
            // half-used connection is dropped, not kept.
            if let Some(mut conn) = slot.take() {
                // Half the budget: a kept connection that hangs (the server
                // moved, the network dropped it) must leave time for a new one.
                if matches!(
                    tokio::time::timeout(budget / 2, select_one(&mut conn)).await,
                    Ok(Ok(()))
                ) {
                    *slot = Some(conn);
                    return Ok(());
                }
            }
            // No kept connection, or the server closed it (idle timeout,
            // restart, pooler). Try once on a new connection.
            let conn = new_connection_ping(&pool).await?;
            *slot = Some(conn);
            drop(slot);
            Ok(())
        })
    }
}

/// Open a new connection with the pool manager and ping on it.
async fn new_connection_ping(pool: &DbPool) -> Result<crate::db::RuntimeConnection, String> {
    use deadpool::managed::Manager as _;

    let mut conn = pool
        .manager()
        .create()
        .await
        .map_err(|error| format!("connection failed: {error}"))?;
    select_one(&mut conn)
        .await
        .map_err(|error| format!("ping failed: {error}"))?;
    Ok(conn)
}

async fn select_one(conn: &mut crate::db::RuntimeConnection) -> Result<(), diesel::result::Error> {
    diesel_async::SimpleAsyncConnection::batch_execute(conn, "SELECT 1").await
}

/// Ping check for one database role (`primary` or `replica`). It caches the
/// result, runs one refresh at a time, and has a time limit.
pub struct DbPingCheck {
    role: &'static str,
    cache: SingleFlightCache<DbPingStatus>,
    timeout_ms: AtomicU64,
    ping: Arc<dyn DbPing>,
    /// Result of the last ping. Used to log a change.
    last_up: Arc<AtomicBool>,
}

impl DbPingCheck {
    /// A check with a 1 s cache and a 2 s time limit.
    pub fn new(role: &'static str) -> Self {
        Self::with_ping(role, Arc::new(DedicatedConnectionPing::default()))
    }

    pub fn with_ping(role: &'static str, ping: Arc<dyn DbPing>) -> Self {
        Self {
            role,
            cache: SingleFlightCache::new(crate::health_cache::DEFAULT_CACHE_TTL),
            timeout_ms: AtomicU64::new(duration_ms(crate::health_cache::DEFAULT_PING_TIMEOUT)),
            ping,
            last_up: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Set the cache TTL (zero turns the cache off) and the time limit.
    pub fn configure(&self, cache_ttl: Duration, timeout: Duration) {
        self.cache.set_ttl(cache_ttl);
        self.timeout_ms
            .store(duration_ms(timeout), Ordering::Relaxed);
    }

    /// Ping the server of `pool`, or return the cached result. A ping that
    /// does not finish in the time limit is down.
    pub async fn check(&self, pool: &DbPool) -> DbPingStatus {
        let role = self.role;
        let timeout_ms = self.timeout_ms.load(Ordering::Relaxed);
        let ping = Arc::clone(&self.ping);
        let pool = pool.clone();
        let last_up = Arc::clone(&self.last_up);
        self.cache
            .get_or_refresh(
                move || async move {
                    let timeout = Duration::from_millis(timeout_ms);
                    let status = match tokio::time::timeout(timeout, ping.ping(pool, timeout)).await
                    {
                        Ok(Ok(())) => DbPingStatus {
                            up: true,
                            error: None,
                        },
                        Ok(Err(error)) => DbPingStatus {
                            up: false,
                            error: Some(format!("{role} {error}")),
                        },
                        Err(_elapsed) => DbPingStatus {
                            up: false,
                            error: Some(format!("{role} ping timed out after {timeout_ms} ms")),
                        },
                    };
                    log_change(role, &last_up, &status);
                    status
                },
                move || DbPingStatus {
                    up: false,
                    error: Some(format!("{role} ping check stopped unexpectedly")),
                },
            )
            .await
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Log when the result changes, so a `503` from `/ready` has a cause in the
/// application log.
fn log_change(role: &str, last_up: &AtomicBool, status: &DbPingStatus) {
    let was_up = last_up.swap(status.up, Ordering::Relaxed);
    match (was_up, status.up) {
        (true, false) => tracing::warn!(
            database = role,
            error = status.error.as_deref().unwrap_or_default(),
            "database readiness ping failed"
        ),
        (false, true) => tracing::info!(database = role, "database readiness ping recovered"),
        _ => {}
    }
}

impl std::fmt::Debug for DbPingCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbPingCheck")
            .field("role", &self.role)
            .field("timeout_ms", &self.timeout_ms)
            .finish_non_exhaustive()
    }
}
