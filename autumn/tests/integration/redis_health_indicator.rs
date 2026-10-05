//! Redis `PING` health indicator against a real Redis (issue #3059).
//!
//! Docker tests: the CI Docker sweep runs them (`--ignored`).

use std::time::{Duration, Instant};

use autumn_web::actuator::{HealthIndicator as _, HealthStatus};
use autumn_web::config::AutumnConfig;
use autumn_web::redis_health::RedisHealthIndicator;
use autumn_web::security::RateLimitBackend;
use autumn_web::test::TestApp;
use testcontainers::ContainerAsync;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::redis::Redis as RedisImage;

async fn start_redis() -> (ContainerAsync<RedisImage>, String) {
    let container = RedisImage::default()
        .start()
        .await
        .expect("start Redis container");
    let port = container
        .get_host_port_ipv4(6379)
        .await
        .expect("redis port");
    (container, format!("redis://127.0.0.1:{port}"))
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ping_to_live_redis_is_up() {
    let (_container, url) = start_redis().await;
    let indicator = RedisHealthIndicator::new(&url).expect("valid url");

    let output = indicator.check().await;

    assert_eq!(output.status, HealthStatus::Up, "{:?}", output.details);
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn ping_is_down_within_timeout_after_redis_stops() {
    let (container, url) = start_redis().await;
    let timeout = Duration::from_millis(500);
    let indicator = RedisHealthIndicator::new(&url)
        .expect("valid url")
        .with_timeout(timeout);
    assert_eq!(indicator.check().await.status, HealthStatus::Up);

    container.stop().await.expect("stop Redis");
    let started = Instant::now();
    let output = indicator.check().await;
    let elapsed = started.elapsed();

    assert_eq!(output.status, HealthStatus::Down, "{:?}", output.details);
    assert!(
        elapsed < timeout + Duration::from_secs(1),
        "PING took {elapsed:?}; the timeout is {timeout:?}"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn indicator_recovers_when_redis_comes_back() {
    let (container, url) = start_redis().await;
    let indicator = RedisHealthIndicator::new(&url)
        .expect("valid url")
        .with_timeout(Duration::from_millis(500));
    assert_eq!(indicator.check().await.status, HealthStatus::Up);

    container.pause().await.expect("pause Redis");
    assert_eq!(indicator.check().await.status, HealthStatus::Down);
    container.unpause().await.expect("unpause Redis");

    let recovered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if indicator.check().await.status == HealthStatus::Up {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(recovered.is_ok(), "indicator did not recover");
}

fn rate_limit_on_redis(url: &str) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.health.detailed = true;
    config.health.ping_timeout_ms = 500;
    // No cache: each request sees the state of Redis now.
    config.health.cache_ttl_ms = 0;
    config.security.rate_limit.enabled = true;
    config.security.rate_limit.backend = RateLimitBackend::Redis;
    config.security.rate_limit.redis.url = Some(url.to_owned());
    config
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn app_reports_redis_subsystem_in_actuator_health_only_by_default() {
    let (container, url) = start_redis().await;
    let client = TestApp::new().config(rate_limit_on_redis(&url)).build();

    let health = client.get("/actuator/health").send().await;
    health.assert_ok();
    health.assert_json::<serde_json::Value, _>(|body| {
        assert_eq!(body["components"]["redis:rate_limit"]["status"], "UP");
    });

    container.stop().await.expect("stop Redis");
    let health = client.get("/actuator/health").send().await;
    health.assert_json::<serde_json::Value, _>(|body| {
        assert_eq!(body["components"]["redis:rate_limit"]["status"], "DOWN");
    });
    // Health-only: a shared Redis outage does not take the replica out.
    client.get("/ready").send().await.assert_ok();
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn redis_readiness_config_makes_redis_gate_ready() {
    let (container, url) = start_redis().await;
    let mut config = rate_limit_on_redis(&url);
    config.health.redis_readiness = true;
    let client = TestApp::new().config(config).build();
    client.get("/ready").send().await.assert_ok();

    container.stop().await.expect("stop Redis");

    client.get("/ready").send().await.assert_status(503);
}

/// `TestApp` always uses in-process channels, so a Redis channels config is
/// not a dependency there and gets no indicator. No Docker: nothing connects.
#[cfg(feature = "ws")]
#[tokio::test]
async fn test_app_with_redis_channels_config_has_no_channels_indicator() {
    let mut config = AutumnConfig::default();
    config.health.detailed = true;
    config.channels.backend = autumn_web::config::ChannelBackend::Redis;
    config.channels.redis.url = Some("redis://127.0.0.1:1".to_owned());
    let client = TestApp::new().config(config).build();

    let health = client.get("/actuator/health").send().await;
    health.assert_json::<serde_json::Value, _>(|body| {
        assert!(body["components"].get("redis:channels").is_none(), "{body}");
    });
}
