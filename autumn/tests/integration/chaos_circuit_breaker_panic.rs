use autumn_web::circuit_breaker::{CircuitBreaker, CircuitBreakerPolicy};
use std::time::Duration;

#[tokio::test]
async fn test_circuit_breaker_panic_on_huge_duration() {
    let policy = CircuitBreakerPolicy {
        failure_ratio_threshold: 0.5,
        sample_window: Duration::from_secs(10),
        minimum_sample_count: 1,
        open_duration: Duration::MAX,
        half_open_trial_count: 2,
    };
    let breaker = CircuitBreaker::new("panic_test", policy);

    // Trigger failure
    let res: Result<(), _> = breaker.run(async { Err("error") }).await;
    assert!(res.is_err());
}
