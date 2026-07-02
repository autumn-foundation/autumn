#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn circuit_breaker_overflow_panic() {
        let policy = crate::circuit_breaker::CircuitBreakerPolicy {
            failure_ratio_threshold: 0.1,
            sample_window: Duration::from_secs(10),
            minimum_sample_count: 1,
            open_duration: Duration::MAX, // Massive duration!
            half_open_trial_count: 1,
        };
        let breaker = crate::circuit_breaker::CircuitBreaker::new("havoc", policy);

        let _ = breaker.before_call();
        breaker.after_call(false); // Triggers open_until = Some(now + open_duration);
    }
}
