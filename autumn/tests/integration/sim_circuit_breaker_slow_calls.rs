//! Issue #3060: the circuit breaker trips on slow calls and counts cancelled
//! calls.
//!
//! Each test reads its policy from TOML. On a breaker without slow-call
//! support, serde ignores the unknown keys. The test then compiles, and it
//! fails because the breaker stays closed.

use std::time::Duration;

use autumn_web::circuit_breaker::{
    CircuitBreaker, CircuitBreakerError, CircuitBreakerPolicy, CircuitState,
};
use autumn_web::config::ResilienceConfig;
use autumn_web::sim::Sim;
use autumn_web::sim_test;

/// A breaker with the policy that `toml` sets under
/// `[circuit_breaker.defaults]`.
fn breaker(name: &str, toml: &str) -> CircuitBreaker {
    let rc: ResilienceConfig = toml::from_str(toml).expect("valid resilience TOML");
    CircuitBreaker::new(name, CircuitBreakerPolicy::from_config(&rc, name))
}

/// An upstream that takes `latency` and never fails.
async fn upstream(latency: Duration) -> Result<(), &'static str> {
    tokio::time::sleep(latency).await;
    Ok(())
}

#[sim_test]
async fn sim_circuit_breaker_slow_upstream_trips_on_slow_call_ratio(_sim: Sim) {
    let breaker = breaker(
        "sim-slow-upstream",
        r"
        [circuit_breaker.defaults]
        sample_window_secs = 60
        minimum_sample_count = 4
        slow_call_duration_threshold_ms = 5000
        slow_call_rate_threshold = 0.5
        ",
    );
    let fast = Duration::from_millis(100);
    let slow = Duration::from_secs(10);

    for _ in 0..3 {
        breaker.run(upstream(fast)).await.expect("fast call");
    }
    // Slow ratios 1/4 and 2/5 are below the threshold.
    for _ in 0..2 {
        breaker.run(upstream(slow)).await.expect("slow call");
        assert_eq!(breaker.state(), CircuitState::Closed);
    }
    // The slow ratio is now 3/6, which reaches the threshold.
    breaker.run(upstream(slow)).await.expect("slow call");
    assert_eq!(
        breaker.state(),
        CircuitState::Open,
        "calls that never fail but take 10 s must trip the breaker"
    );

    let mut called = false;
    let res = breaker
        .run(async {
            called = true;
            upstream(slow).await
        })
        .await;
    assert!(matches!(res, Err(CircuitBreakerError::Open)));
    assert!(!called, "an open breaker must not call the upstream");
}

#[sim_test]
async fn sim_circuit_breaker_cancelled_calls_count_as_slow(_sim: Sim) {
    let breaker = breaker(
        "sim-cancelled-slow",
        r"
        [circuit_breaker.defaults]
        sample_window_secs = 300
        minimum_sample_count = 4
        slow_call_duration_threshold_ms = 5000
        slow_call_rate_threshold = 0.5
        ",
    );
    // A request timeout cancels each call after 30 s.
    for _ in 0..4 {
        let res = tokio::time::timeout(
            Duration::from_secs(30),
            breaker.run(upstream(Duration::MAX)),
        )
        .await;
        assert!(res.is_err(), "the timeout cancels the call");
    }
    assert_eq!(
        breaker.state(),
        CircuitState::Open,
        "calls cancelled after the slow threshold must trip the breaker"
    );
}

#[sim_test]
async fn sim_circuit_breaker_cancelled_calls_count_as_failures(_sim: Sim) {
    let breaker = breaker(
        "sim-cancelled-failure",
        r#"
        [circuit_breaker.defaults]
        sample_window_secs = 300
        minimum_sample_count = 4
        slow_call_duration_threshold_ms = 5000
        cancelled_call_outcome = "failure"
        "#,
    );
    for _ in 0..3 {
        let res = tokio::time::timeout(
            Duration::from_secs(30),
            breaker.run(upstream(Duration::MAX)),
        )
        .await;
        assert!(res.is_err(), "the timeout cancels the call");
    }
    assert!(
        (breaker.failure_ratio() - 1.0).abs() < f64::EPSILON,
        "a call cancelled after the slow threshold is a failure, got ratio {}",
        breaker.failure_ratio()
    );
    assert_eq!(breaker.state(), CircuitState::Closed, "3 of 4 samples");

    let res = tokio::time::timeout(
        Duration::from_secs(30),
        breaker.run(upstream(Duration::MAX)),
    )
    .await;
    assert!(res.is_err());
    assert_eq!(breaker.state(), CircuitState::Open);
}

#[sim_test]
async fn sim_circuit_breaker_fast_cancel_records_nothing(_sim: Sim) {
    let breaker = breaker(
        "sim-fast-cancel",
        r#"
        [circuit_breaker.defaults]
        minimum_sample_count = 1
        slow_call_duration_threshold_ms = 5000
        cancelled_call_outcome = "failure"
        "#,
    );
    // A client that goes away after 1 s is not a fault of the upstream.
    let res =
        tokio::time::timeout(Duration::from_secs(1), breaker.run(upstream(Duration::MAX))).await;
    assert!(res.is_err());
    assert!(breaker.failure_ratio() < f64::EPSILON);
    assert_eq!(breaker.state(), CircuitState::Closed);
}
