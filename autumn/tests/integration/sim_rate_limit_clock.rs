//! Sim-testing follow-on (issue #1797): the token-bucket rate limiter reads its
//! refill clock from the framework's injected [`ClockSource`], so a
//! deterministic `#[sim_test]` can **exhaust** a bucket and then **refill** it
//! under virtual time — with zero real sleep.
//!
//! The limiter's per-key bucket refills at `requests_per_second` tokens/sec off
//! the injected clock. Configuring a small burst with a slow refill, the test
//! drains the bucket (the over-burst request is thereby `429`d), advances the
//! sim clock past one refill window, and shows the next request is allowed
//! again. Because refill is driven by the sim's virtual clock (stepped by
//! [`Sim::advance`]), the whole exhaust→refill cycle is deterministic and
//! sleeps for no wall-clock time.

use std::time::Duration;

use autumn_web::config::AutumnConfig;
use autumn_web::sim::Sim;
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};

/// Trivial always-200 route to hang the rate limiter off of.
#[get("/ping")]
async fn ping() -> &'static str {
    "pong"
}

/// Enable the built-in per-IP rate limiter with an explicit `burst` and a slow
/// `requests_per_second` refill. Keying is via `X-Forwarded-For` so the test
/// needs no real TCP peer (mirrors `rate_limit_pipeline.rs`).
fn rate_limited(rps: f64, burst: u32) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.security.rate_limit.enabled = true;
    config.security.rate_limit.requests_per_second = rps;
    config.security.rate_limit.burst = burst;
    config.security.rate_limit.trust_forwarded_headers = true;
    config
}

/// Stable per-IP bucket key (via `X-Forwarded-For`) for the whole scenario.
const CLIENT_IP: &str = "198.51.100.42";

#[sim_test]
async fn bucket_exhausts_then_refills_under_virtual_time(mut sim: Sim) {
    // Seed-stable and deterministic: the sim always starts at seed 0.
    assert_eq!(sim.seed, 0);

    // burst = 2 tokens, refill = 0.1 tokens/sec (one token every 10 virtual
    // seconds). The limiter reads refill time off the sim's injected clock.
    let client = sim.build(
        TestApp::new()
            .routes(routes![ping])
            .config(rate_limited(0.1, 2)),
    );

    let ping = |c: &autumn_web::test::TestClient| {
        c.get("/ping").header("X-Forwarded-For", CLIENT_IP).send()
    };

    // Drain the burst: the first two requests consume the two tokens.
    ping(client).await.assert_status(200);
    ping(client).await.assert_status(200);

    // Bucket now empty and refill is slow, so the next request is rejected with
    // the standard 429 + `Retry-After`.
    let throttled = ping(client).await;
    throttled.assert_status(429);
    assert!(
        throttled.header("retry-after").is_some(),
        "a throttled response must carry a Retry-After header",
    );
    throttled.assert_header("x-ratelimit-remaining", "0");

    // Still throttled immediately after, before any virtual time passes.
    ping(client).await.assert_status(429);

    // Jump the virtual clock forward one full refill window (20s ⇒ 2 tokens at
    // 0.1/sec, capped at the burst of 2). No wall-clock sleep occurs — the sim
    // clock the limiter reads simply reports a later instant.
    sim.advance(Duration::from_secs(20)).await;

    // The bucket refilled deterministically under the sim clock: allowed again.
    ping(sim.client()).await.assert_status(200);
    // ...and a second token was available too (burst refilled to the cap).
    ping(sim.client()).await.assert_status(200);
    // Third over-burst request is throttled once more — refill really is bounded
    // by the advanced clock, not an unconditional reset.
    ping(sim.client()).await.assert_status(429);
}
