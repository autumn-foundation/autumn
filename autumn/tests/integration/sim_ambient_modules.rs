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

fn idempotency_record() -> autumn_web::idempotency::IdempotencyRecord {
    autumn_web::idempotency::IdempotencyRecord {
        status: 200,
        headers: Vec::new(),
        body: b"ok".to_vec(),
        metadata: Vec::new(),
    }
}

#[sim_test]
async fn sim_ambient_modules_idempotency_ttl_runs_on_into_the_next_sim(sim: Sim) {
    use autumn_web::idempotency::{IdempotencyStore, MemoryIdempotencyStore};

    let minute = Duration::from_secs(60);
    let store = MemoryIdempotencyStore::new(minute);
    let first = Sim::from_seed(sim.seed.wrapping_add(1));
    first.advance(Duration::from_secs(3600)).await;
    store
        .set("k", idempotency_record(), b"hash".to_vec(), minute)
        .await
        .unwrap();
    assert!(store.try_lock("lock", "a", minute).await.unwrap());
    drop(first);

    // The next sim's instants start after the first sim's. The entry and the
    // lock live out their minute there, and no longer.
    let second = Sim::from_seed(sim.seed.wrapping_add(2));
    assert!(store.get("k").await.unwrap().is_some(), "inside its TTL");
    assert!(
        !store.try_lock("lock", "b", minute).await.unwrap(),
        "still held"
    );
    second.advance(minute + Duration::from_secs(1)).await;
    assert!(store.get("k").await.unwrap().is_none(), "its TTL ran out");
    assert!(
        store.try_lock("lock", "b", minute).await.unwrap(),
        "the lock ran out"
    );
}

#[sim_test]
async fn sim_ambient_modules_idempotency_ttl_runs_on_after_a_nested_sim(sim: Sim) {
    use autumn_web::idempotency::{IdempotencyStore, MemoryIdempotencyStore};

    let minute = Duration::from_secs(60);
    let store = MemoryIdempotencyStore::new(minute);
    let inner = Sim::from_seed(sim.seed.wrapping_add(1));
    inner.advance(Duration::from_secs(3600)).await;
    store
        .set("k", idempotency_record(), b"hash".to_vec(), minute)
        .await
        .unwrap();
    assert!(store.try_lock("lock", "a", minute).await.unwrap());
    drop(inner);

    // The outer sim's instants go on from the nested sim's. The entry and
    // the lock live out their minute here, and no longer.
    assert!(store.get("k").await.unwrap().is_some(), "inside its TTL");
    assert!(
        !store.try_lock("lock", "b", minute).await.unwrap(),
        "still held"
    );
    sim.advance(minute + Duration::from_secs(1)).await;
    assert!(store.get("k").await.unwrap().is_none(), "its TTL ran out");
    assert!(
        store.try_lock("lock", "b", minute).await.unwrap(),
        "the lock ran out"
    );
}

#[sim_test]
async fn sim_ambient_modules_tenant_cells_idle_out_in_the_next_sim(sim: Sim) {
    use autumn_web::TenantCellRegistry;

    let minute = Duration::from_secs(60);
    let registry = TenantCellRegistry::with_limits(0, Some(minute));
    let first = Sim::from_seed(sim.seed.wrapping_add(1));
    first.advance(Duration::from_secs(3600)).await;
    let _cell = registry.get_or_create("tenant", 1024);
    drop(first);

    // The next sim's instants start after the first sim's, so the cell's
    // idle age runs on there.
    let second = Sim::from_seed(sim.seed.wrapping_add(2));
    assert_eq!(registry.evict_idle_older_than(minute), 0, "not idle yet");
    second.advance(minute + Duration::from_secs(1)).await;
    assert_eq!(
        registry.evict_idle_older_than(minute),
        1,
        "idle past its TTL"
    );
}
