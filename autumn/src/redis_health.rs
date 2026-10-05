//! Redis health indicator: a `PING` with a time limit (issue #3059).
//!
//! The framework registers one indicator for each subsystem whose config
//! selects the Redis backend, named `redis:<subsystem>`. By default they are
//! [`IndicatorGroup::HealthOnly`]: Redis is shared by all replicas, so a
//! Redis failure must not take every replica out of rotation at the same
//! time. Set `health.redis_readiness = true` to make them gate `/ready`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use redis::aio::MultiplexedConnection;

use crate::actuator::{
    HealthCheckOutput, HealthIndicator, HealthIndicatorRegistry, HealthStatus, IndicatorGroup,
};
use crate::config::AutumnConfig;

/// Time the registry gives the indicator after its own `PING` limit. The
/// indicator then reports its own `DOWN` result, not the registry's
/// `UNKNOWN` timeout result.
const REGISTRY_TIMEOUT_MARGIN: Duration = Duration::from_millis(500);

/// One Redis server and the connection kept for `PING`. Indicators for the
/// same URL share it.
struct RedisPinger {
    client: redis::Client,
    connection: tokio::sync::Mutex<Option<MultiplexedConnection>>,
}

impl RedisPinger {
    fn new(url: &str) -> redis::RedisResult<Self> {
        Ok(Self {
            client: crate::redis_tls::open_client(url)?,
            connection: tokio::sync::Mutex::new(None),
        })
    }

    /// Send `PING`. Use the kept connection first, with half the budget. If
    /// it fails, open a new connection once.
    async fn ping(&self, budget: Duration) -> Result<(), String> {
        // Wait for a ping of another indicator on this URL. It is short, and
        // the caller's time limit bounds the wait.
        let mut slot = self.connection.lock().await;
        // Take the connection out first. If this future is cancelled, a
        // half-used connection is dropped, not kept.
        if let Some(mut connection) = slot.take()
            && matches!(
                tokio::time::timeout(budget / 2, send_ping(&mut connection)).await,
                Ok(Ok(()))
            )
        {
            *slot = Some(connection);
            return Ok(());
        }
        // A kept connection that fails can be half-open (the server moved
        // or the network dropped it). Do not use it again.
        let config = redis::AsyncConnectionConfig::new()
            .set_connection_timeout(Some(budget))
            .set_response_timeout(Some(budget));
        let mut connection = self
            .client
            .get_multiplexed_async_connection_with_config(&config)
            .await
            .map_err(|error| format!("connection failed: {error}"))?;
        send_ping(&mut connection)
            .await
            .map_err(|error| format!("PING failed: {error}"))?;
        *slot = Some(connection);
        drop(slot);
        Ok(())
    }
}

async fn send_ping(connection: &mut MultiplexedConnection) -> redis::RedisResult<()> {
    redis::cmd("PING")
        .query_async::<String>(connection)
        .await
        .map(drop)
}

/// Sends `PING` to Redis. `DOWN` on error or when the time limit expires.
///
/// The indicator keeps one connection. It drops a connection that fails and
/// opens a new one on the next check. Building the indicator does no I/O and
/// needs no Tokio runtime.
///
/// # Example
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use std::time::Duration;
/// use autumn_web::redis_health::RedisHealthIndicator;
///
/// # fn demo() -> redis::RedisResult<()> {
/// let indicator = RedisHealthIndicator::new("redis://127.0.0.1:6379")?
///     .with_timeout(Duration::from_millis(500));
/// let _app = autumn_web::app().health_indicator("redis:custom", Arc::new(indicator));
/// # Ok(()) }
/// ```
pub struct RedisHealthIndicator {
    pinger: Arc<RedisPinger>,
    timeout: Duration,
    group: IndicatorGroup,
}

impl RedisHealthIndicator {
    /// Build an indicator for `url`. The `PING` time limit is 2 s and the
    /// group is [`IndicatorGroup::HealthOnly`].
    ///
    /// # Errors
    ///
    /// Returns an error when `url` is not a valid Redis URL.
    pub fn new(url: &str) -> redis::RedisResult<Self> {
        Ok(Self::sharing(Arc::new(RedisPinger::new(url)?)))
    }

    const fn sharing(pinger: Arc<RedisPinger>) -> Self {
        Self {
            pinger,
            timeout: crate::health_cache::DEFAULT_PING_TIMEOUT,
            group: IndicatorGroup::HealthOnly,
        }
    }

    /// Set the `PING` time limit. It also limits the connect time.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set the probe group.
    #[must_use]
    pub const fn with_group(mut self, group: IndicatorGroup) -> Self {
        self.group = group;
        self
    }
}

impl std::fmt::Debug for RedisHealthIndicator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisHealthIndicator")
            .field("timeout", &self.timeout)
            .field("group", &self.group)
            .finish_non_exhaustive()
    }
}

impl HealthIndicator for RedisHealthIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, HealthCheckOutput> {
        Box::pin(async move {
            let mut details = HashMap::new();
            let status =
                match tokio::time::timeout(self.timeout, self.pinger.ping(self.timeout)).await {
                    Ok(Ok(())) => HealthStatus::Up,
                    Ok(Err(error)) => {
                        details.insert("error".to_owned(), serde_json::json!(error));
                        HealthStatus::Down
                    }
                    Err(_elapsed) => {
                        details.insert("timed_out".to_owned(), serde_json::Value::Bool(true));
                        HealthStatus::Down
                    }
                };
            HealthCheckOutput { status, details }
        })
    }

    fn timeout_ms(&self) -> u64 {
        u64::try_from((self.timeout + REGISTRY_TIMEOUT_MARGIN).as_millis()).unwrap_or(u64::MAX)
    }

    fn group(&self) -> IndicatorGroup {
        self.group
    }
}

/// Each enabled Redis-backed subsystem and its URL, in a stable order.
pub(crate) fn redis_subsystems(config: &AutumnConfig) -> Vec<(&'static str, String)> {
    use crate::config::IdempotencyBackend;

    let idempotency_redis = config.idempotency.backend == IdempotencyBackend::Redis;
    let candidates: [(&'static str, bool, &Option<String>); 8] = [
        ("cache", config.cache.is_redis(), &config.cache.redis.url),
        (
            "channels",
            cfg!(feature = "ws") && config.channels.backend == crate::config::ChannelBackend::Redis,
            &config.channels.redis.url,
        ),
        (
            "idempotency",
            idempotency_redis && config.idempotency.enabled != Some(false),
            &config.idempotency.redis.url,
        ),
        (
            "jobs",
            config.jobs.backend.trim().eq_ignore_ascii_case("redis"),
            &config.jobs.redis.url,
        ),
        (
            "rate_limit",
            config.security.rate_limit.enabled
                && config.security.rate_limit.backend
                    == crate::security::config::RateLimitBackend::Redis,
            &config.security.rate_limit.redis.url,
        ),
        (
            "sessions",
            config.session.backend == crate::session::SessionBackend::Redis,
            &config.session.redis.url,
        ),
        (
            "submit_token",
            config.security.submit_token.enabled
                && config
                    .security
                    .submit_token
                    .resolved_backend(config.idempotency.backend)
                    == IdempotencyBackend::Redis,
            &config.idempotency.redis.url,
        ),
        (
            "webhook_replay",
            config.security.webhooks.replay.backend == crate::webhook::WebhookReplayBackend::Redis,
            &config.security.webhooks.replay.redis.url,
        ),
    ];

    candidates
        .into_iter()
        .filter(|(_, enabled, _)| *enabled)
        .filter_map(|(name, _, url)| {
            let url = url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())?;
            Some((name, url.to_owned()))
        })
        .collect()
}

/// Register a `redis:<subsystem>` indicator for each enabled Redis-backed
/// subsystem. Logs and skips a subsystem whose URL is not valid.
pub(crate) fn register_redis_health_indicators(
    config: &AutumnConfig,
    registry: &HealthIndicatorRegistry,
) {
    let group = if config.health.redis_readiness {
        IndicatorGroup::Readiness
    } else {
        IndicatorGroup::HealthOnly
    };
    // One kept connection per Redis server, not per subsystem.
    let mut pingers: HashMap<String, Arc<RedisPinger>> = HashMap::new();
    for (subsystem, url) in redis_subsystems(config) {
        let name = format!("redis:{subsystem}");
        let pinger = if let Some(pinger) = pingers.get(&url) {
            Arc::clone(pinger)
        } else {
            match RedisPinger::new(&url) {
                Ok(pinger) => {
                    let pinger = Arc::new(pinger);
                    pingers.insert(url.clone(), Arc::clone(&pinger));
                    pinger
                }
                Err(error) => {
                    tracing::warn!(
                        indicator = %name,
                        url = %crate::redis_tls::redact_url(&url),
                        error = %error,
                        "Redis health indicator not registered: URL is not valid"
                    );
                    continue;
                }
            }
        };
        let indicator = RedisHealthIndicator::sharing(pinger)
            .with_timeout(config.health.ping_timeout())
            .with_group(group);
        if let Err(error) = registry.register(name, group, Arc::new(indicator)) {
            tracing::warn!("{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subsystem_uses_redis_by_default() {
        assert!(redis_subsystems(&AutumnConfig::default()).is_empty());
    }

    #[test]
    fn each_redis_backed_subsystem_is_listed() {
        let mut config = AutumnConfig::default();
        config.jobs.backend = "redis".to_owned();
        config.jobs.redis.url = Some("redis://jobs:6379".to_owned());
        config.session.backend = crate::session::SessionBackend::Redis;
        config.session.redis.url = Some("redis://sessions:6379".to_owned());
        config.security.rate_limit.enabled = true;
        config.security.rate_limit.backend = crate::security::config::RateLimitBackend::Redis;
        config.security.rate_limit.redis.url = Some("redis://rate:6379".to_owned());
        config.idempotency.backend = crate::config::IdempotencyBackend::Redis;
        config.idempotency.redis.url = Some("redis://idem:6379".to_owned());
        config.security.webhooks.replay.backend = crate::webhook::WebhookReplayBackend::Redis;
        config.security.webhooks.replay.redis.url = Some("redis://hooks:6379".to_owned());
        config.cache.backend = crate::config::CacheBackend::Redis;
        config.cache.redis.url = Some("redis://cache:6379".to_owned());
        #[cfg(feature = "ws")]
        {
            config.channels.backend = crate::config::ChannelBackend::Redis;
            config.channels.redis.url = Some("redis://channels:6379".to_owned());
        }

        let names: Vec<_> = redis_subsystems(&config)
            .into_iter()
            .map(|(name, url)| format!("{name}={url}"))
            .collect();

        let expected: Vec<&str> = [
            "cache=redis://cache:6379",
            "channels=redis://channels:6379",
            "idempotency=redis://idem:6379",
            "jobs=redis://jobs:6379",
            "rate_limit=redis://rate:6379",
            "sessions=redis://sessions:6379",
            "submit_token=redis://idem:6379",
            "webhook_replay=redis://hooks:6379",
        ]
        .into_iter()
        // Channels use Redis only in a `ws` build.
        .filter(|entry| cfg!(feature = "ws") || !entry.starts_with("channels="))
        .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn disabled_or_url_less_subsystems_are_skipped() {
        let mut config = AutumnConfig::default();
        // Rate limiting selects Redis but is off.
        config.security.rate_limit.backend = crate::security::config::RateLimitBackend::Redis;
        config.security.rate_limit.redis.url = Some("redis://rate:6379".to_owned());
        // Idempotency selects Redis but is switched off by the operator.
        config.idempotency.enabled = Some(false);
        config.idempotency.backend = crate::config::IdempotencyBackend::Redis;
        config.idempotency.redis.url = Some("redis://idem:6379".to_owned());
        config.security.submit_token.enabled = false;
        // Sessions select Redis with a blank URL.
        config.session.backend = crate::session::SessionBackend::Redis;
        config.session.redis.url = Some("   ".to_owned());

        assert!(redis_subsystems(&config).is_empty());
    }

    #[tokio::test]
    async fn registration_follows_readiness_config() {
        let mut config = AutumnConfig::default();
        config.session.backend = crate::session::SessionBackend::Redis;
        config.session.redis.url = Some("redis://127.0.0.1:1".to_owned());

        let health_only = HealthIndicatorRegistry::new();
        register_redis_health_indicators(&config, &health_only);
        assert!(health_only.contains("redis:sessions"));
        assert!(health_only.run_readiness().await.is_empty());

        config.health.redis_readiness = true;
        let readiness = HealthIndicatorRegistry::new();
        register_redis_health_indicators(&config, &readiness);
        let results = readiness.run_readiness().await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "redis:sessions");
    }

    #[test]
    fn invalid_url_is_rejected() {
        assert!(RedisHealthIndicator::new("not a redis url").is_err());
    }

    #[tokio::test]
    async fn refused_connection_is_down() {
        let addr = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("local addr")
        };
        let indicator = RedisHealthIndicator::new(&format!("redis://{addr}"))
            .expect("valid url")
            .with_timeout(Duration::from_secs(2));

        let output = indicator.check().await;

        assert_eq!(output.status, HealthStatus::Down);
        assert!(output.details.contains_key("error"), "{:?}", output.details);
    }

    #[tokio::test]
    async fn hung_server_is_down_within_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                // Hold the socket open and never answer.
                tokio::spawn(async move {
                    let _socket = socket;
                    std::future::pending::<()>().await;
                });
            }
        });
        let timeout = Duration::from_millis(300);
        let indicator = RedisHealthIndicator::new(&format!("redis://{addr}"))
            .expect("valid url")
            .with_timeout(timeout);

        let started = std::time::Instant::now();
        let output = indicator.check().await;
        let elapsed = started.elapsed();

        assert_eq!(output.status, HealthStatus::Down);
        // The connect limit of redis-rs and the outer limit are equal. Either
        // can end the check first.
        assert!(
            output.details.contains_key("timed_out") || output.details.contains_key("error"),
            "{:?}",
            output.details
        );
        assert!(
            elapsed < timeout + Duration::from_secs(1),
            "took {elapsed:?}"
        );
        server.abort();
    }

    #[test]
    fn indicator_is_built_without_a_runtime() {
        let indicator = RedisHealthIndicator::new("redis://127.0.0.1:1")
            .expect("valid url")
            .with_timeout(Duration::from_millis(700));

        assert_eq!(indicator.timeout_ms(), 1_200);
        assert_eq!(indicator.group(), IndicatorGroup::HealthOnly);
    }

    /// A small RESP server. It answers `PING` with `+PONG` after `delay`, and
    /// other commands with `+OK`. A connection made before the last
    /// [`FakeRedis::mute_open_connections`] call stops answering.
    struct FakeRedis {
        addr: std::net::SocketAddr,
        accepted: Arc<std::sync::atomic::AtomicUsize>,
        generation: Arc<std::sync::atomic::AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeRedis {
        async fn start(delay: Duration) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("local addr");
            let accepted = Arc::new(AtomicUsize::new(0));
            let generation = Arc::new(AtomicUsize::new(0));
            let (count, current) = (Arc::clone(&accepted), Arc::clone(&generation));
            let task = tokio::spawn(async move {
                while let Ok((socket, _)) = listener.accept().await {
                    count.fetch_add(1, Ordering::SeqCst);
                    let born = current.load(Ordering::SeqCst);
                    let current = Arc::clone(&current);
                    tokio::spawn(async move {
                        let (read, mut write) = socket.into_split();
                        let mut read = tokio::io::BufReader::new(read);
                        let mut line = String::new();
                        loop {
                            // One command: `*<n>`, then `$<len>` + data, n times.
                            line.clear();
                            if read.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            let parts: usize = line.trim()[1..].parse().unwrap_or(0);
                            let mut words = Vec::new();
                            for _ in 0..parts {
                                line.clear();
                                let _ = read.read_line(&mut line).await;
                                let len: usize = line.trim()[1..].parse().unwrap_or(0);
                                let mut word = vec![0; len + 2];
                                let _ = read.read_exact(&mut word).await;
                                words.push(String::from_utf8_lossy(&word[..len]).to_uppercase());
                            }
                            if current.load(Ordering::SeqCst) != born {
                                continue; // muted: never answer
                            }
                            let reply: &[u8] = if words.first().is_some_and(|w| w == "PING") {
                                tokio::time::sleep(delay).await;
                                b"+PONG\r\n"
                            } else {
                                b"+OK\r\n"
                            };
                            if write.write_all(reply).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            Self {
                addr,
                accepted,
                generation,
                task,
            }
        }

        fn url(&self) -> String {
            format!("redis://{}", self.addr)
        }

        fn accepted(&self) -> usize {
            self.accepted.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn mute_open_connections(&self) {
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl Drop for FakeRedis {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn slow_ping_inside_the_time_limit_is_up() {
        // redis-rs has a 500 ms response limit by default. The indicator must
        // use its own limit.
        let redis = FakeRedis::start(Duration::from_millis(700)).await;
        let indicator = RedisHealthIndicator::new(&redis.url())
            .expect("valid url")
            .with_timeout(Duration::from_secs(3));

        let output = indicator.check().await;

        assert_eq!(output.status, HealthStatus::Up, "{:?}", output.details);
    }

    #[tokio::test]
    async fn half_open_connection_is_replaced() {
        let redis = FakeRedis::start(Duration::ZERO).await;
        let indicator = RedisHealthIndicator::new(&redis.url())
            .expect("valid url")
            .with_timeout(Duration::from_secs(1));
        assert_eq!(indicator.check().await.status, HealthStatus::Up);
        assert_eq!(redis.accepted(), 1);

        // The kept connection stops answering, as after a failover.
        redis.mute_open_connections();
        let output = indicator.check().await;

        assert_eq!(output.status, HealthStatus::Up, "{:?}", output.details);
        assert_eq!(
            redis.accepted(),
            2,
            "a new connection replaced the mute one"
        );
    }

    #[tokio::test]
    async fn subsystems_on_one_url_share_one_connection() {
        let redis = FakeRedis::start(Duration::ZERO).await;
        let mut config = AutumnConfig::default();
        config.idempotency.backend = crate::config::IdempotencyBackend::Redis;
        config.idempotency.redis.url = Some(redis.url());
        config.session.backend = crate::session::SessionBackend::Redis;
        config.session.redis.url = Some(redis.url());
        let registry = HealthIndicatorRegistry::new();
        register_redis_health_indicators(&config, &registry);

        let results = registry.run_all().await;

        let redis_results: Vec<_> = results
            .iter()
            .filter(|r| r.name.starts_with("redis:"))
            .collect();
        assert_eq!(
            redis_results.len(),
            3,
            "idempotency, sessions, submit_token"
        );
        assert!(
            redis_results
                .iter()
                .all(|r| r.output.status == HealthStatus::Up)
        );
        assert_eq!(redis.accepted(), 1);
    }
}
