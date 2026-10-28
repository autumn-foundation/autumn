//! A breaker configured with an unrepresentable `open_duration` must trip
//! open instead of panicking on `Instant + Duration` overflow.

use autumn_web::circuit_breaker::{
    CircuitBreaker, CircuitBreakerError, CircuitBreakerPolicy, CircuitState,
};
use std::time::Duration;

fn huge_open_duration_policy() -> CircuitBreakerPolicy {
    CircuitBreakerPolicy {
        failure_ratio_threshold: 0.5,
        sample_window: Duration::from_secs(10),
        minimum_sample_count: 1,
        open_duration: Duration::MAX,
        half_open_trial_count: 2,
        ..CircuitBreakerPolicy::default()
    }
}

#[tokio::test]
async fn huge_open_duration_trips_open_without_panicking() {
    let breaker = CircuitBreaker::new("panic_test", huge_open_duration_policy());

    let res: Result<(), _> = breaker.run(async { Err("error") }).await;
    assert!(matches!(res, Err(CircuitBreakerError::Execution("error"))));
    assert_eq!(breaker.state(), CircuitState::Open);

    // The clamped deadline is far in the future, so the breaker stays open.
    let res: Result<(), CircuitBreakerError<&str>> = breaker.run(async { Ok(()) }).await;
    assert!(matches!(res, Err(CircuitBreakerError::Open)));
}
