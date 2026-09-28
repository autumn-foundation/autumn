//! Sim Phase 2 (issue #2967): migrated modules follow the `Sim` clock.
//!
//! The circuit breaker had no clock in scope, so it read `Instant::now()`, and
//! its open window ran on real time even inside a `#[sim_test]`. It now reads
//! the ambient clock, so `Sim::advance` moves it.

use std::time::Duration;

use autumn_web::circuit_breaker::{
    CircuitBreaker, CircuitBreakerGuard, CircuitBreakerPolicy, CircuitState,
};
use autumn_web::sim::Sim;
use autumn_web::sim_test;

#[sim_test]
async fn sim_ambient_modules_breaker_opens_and_half_opens_in_virtual_time(sim: Sim) {
    let policy = CircuitBreakerPolicy {
        failure_ratio_threshold: 0.5,
        sample_window: Duration::from_secs(10),
        minimum_sample_count: 2,
        open_duration: Duration::from_secs(3600),
        half_open_trial_count: 1,
    };
    let breaker = CircuitBreaker::new("sim-ambient-breaker", policy);
    for _ in 0..2 {
        CircuitBreakerGuard::new(breaker.clone()).failure();
    }
    assert_eq!(breaker.state(), CircuitState::Open);

    sim.advance(Duration::from_secs(3599)).await;
    assert_eq!(
        breaker.state(),
        CircuitState::Open,
        "one virtual second is left"
    );

    sim.advance(Duration::from_secs(1)).await;
    assert_eq!(
        breaker.state(),
        CircuitState::HalfOpen,
        "an hour of virtual time closes the open window, with no real wait"
    );
}

#[sim_test]
async fn sim_ambient_modules_global_breakers_stay_on_real_time(sim: Sim) {
    // The process-global registry outlives the sim. Its breakers read the
    // system clock, so no virtual instant reaches it.
    let policy = CircuitBreakerPolicy {
        failure_ratio_threshold: 0.5,
        sample_window: Duration::from_secs(10),
        minimum_sample_count: 2,
        open_duration: Duration::from_secs(3600),
        half_open_trial_count: 1,
    };
    let breaker = autumn_web::circuit_breaker::global_registry()
        .get_or_create("sim-ambient-global-breaker", policy);
    for _ in 0..2 {
        CircuitBreakerGuard::new(breaker.clone()).failure();
    }
    assert_eq!(breaker.state(), CircuitState::Open);

    sim.advance(Duration::from_secs(2 * 3600)).await;
    assert_eq!(
        breaker.state(),
        CircuitState::Open,
        "virtual time does not move a global breaker"
    );
}

#[sim_test]
async fn sim_ambient_modules_idempotency_entries_stay_on_their_timeline(sim: Sim) {
    use autumn_web::idempotency::{IdempotencyRecord, IdempotencyStore, MemoryIdempotencyStore};

    let store = MemoryIdempotencyStore::new(Duration::from_secs(60));
    sim.advance(Duration::from_secs(3600)).await;
    let record = IdempotencyRecord {
        status: 200,
        headers: Vec::new(),
        body: b"ok".to_vec(),
        metadata: Vec::new(),
    };
    store.set("k", record, b"hash".to_vec(), Duration::from_secs(60));
    assert!(store.try_lock("lock", Duration::from_secs(60)));
    assert!(store.get("k").is_some(), "fresh on its own timeline");
    assert!(
        !store.try_lock("lock", Duration::from_secs(60)),
        "still held"
    );

    // A new sim starts at zero. The entry and the lock were stored an hour
    // later on another timeline, so they must not outlive their TTL here.
    let inner = Sim::from_seed(sim.seed.wrapping_add(1));
    assert!(
        store.get("k").is_none(),
        "the entry is from another timeline"
    );
    assert!(
        store.try_lock("lock", Duration::from_secs(60)),
        "the lock is from another timeline"
    );
    drop(inner);
}
