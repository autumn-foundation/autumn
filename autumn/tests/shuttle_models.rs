//! Shuttle checks of the real retry budget, tenant bulkhead and circuit
//! breaker.
//!
//! This binary is EMPTY under a normal build (`#![cfg(shuttle_models)]`). It
//! compiles and runs only under `RUSTFLAGS="--cfg shuttle_models"`, in the
//! `Shuttle` CI job:
//!
//! ```text
//! RUSTFLAGS="--cfg shuttle_models" \
//!   cargo test -p autumn-web --release --test shuttle_models
//! ```
//!
//! Under that cfg, `src/sync_shim.rs` gives `RetryBudget`, `RetryBudgets`,
//! `TenantBulkhead` and `CircuitBreaker` Shuttle's locks and atomics. Thus
//! these tests drive the shipped code, not a copy: every lock and atomic in it
//! is a point where Shuttle can switch threads.
//!
//! loom (`tests/loom_models.rs`) explores every interleaving, so it can check
//! only two or three threads with a few steps each. Shuttle samples schedules
//! at random and with PCT (probabilistic concurrency testing, which finds a
//! bug of depth `d` with a known probability). Thus it scales to more threads,
//! more steps and async tasks. The two are complementary.
//!
//! `SHUTTLE_ITERATIONS` sets the schedules for each test (default 2000).
#![cfg(shuttle_models)]

use std::sync::Arc;
use std::time::Duration;

use autumn_web::__concurrency_models::{RetryBudget, RetryBudgets, RetryKind};
use autumn_web::bulkhead::TenantBulkhead;
use autumn_web::circuit_breaker::{CircuitBreaker, CircuitBreakerPolicy, CircuitState};
use autumn_web::config::RetryBudgetConfig;
use shuttle::sync::atomic::{AtomicUsize, Ordering};
use shuttle::{future, thread};

fn iterations() -> usize {
    std::env::var("SHUTTLE_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2_000)
}

/// Run `body` under the random scheduler, then under PCT with depth 3.
fn check(body: impl Fn() + Send + Sync + 'static) {
    let body = Arc::new(body);
    let random = Arc::clone(&body);
    shuttle::check_random(move || random(), iterations());
    shuttle::check_pct(move || body(), iterations(), 3);
}

/// 10 tokens; a transient retry costs 3, a throttled retry costs 2, and a
/// first attempt adds nothing. Thus a full bucket allows exactly three
/// transient retries.
fn small_budget_config() -> RetryBudgetConfig {
    RetryBudgetConfig {
        enabled: true,
        capacity: 10,
        transient_cost: 3,
        throttling_cost: 2,
        retry_ratio: 0.0,
    }
}

fn drain(budget: &RetryBudget, kind: RetryKind) -> usize {
    let mut taken = 0;
    while budget.try_acquire(kind) {
        taken += 1;
    }
    taken
}

/// Four threads race to take transient retries from one bucket. The
/// compare-exchange loop must neither lose an update (more than three grants)
/// nor fail a retry that had tokens (fewer than three).
#[test]
fn retry_budget_grants_exactly_the_capacity() {
    check(|| {
        let budget = Arc::new(RetryBudget::new(&small_budget_config()));
        let granted = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let budget = Arc::clone(&budget);
                let granted = Arc::clone(&granted);
                thread::spawn(move || {
                    for _ in 0..2 {
                        if budget.try_acquire(RetryKind::Transient) {
                            granted.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(granted.load(Ordering::SeqCst), 3);
        assert_eq!(drain(&budget, RetryKind::Transient), 0);
    });
}

/// Retries that succeed give their tokens back. Whatever the order of the
/// takes and gives across threads, the bucket ends where it started, and never
/// above its capacity.
#[test]
fn retry_budget_release_conserves_tokens() {
    check(|| {
        let budget = Arc::new(RetryBudget::new(&small_budget_config()));
        let workers: Vec<_> = [
            RetryKind::Transient,
            RetryKind::Throttling,
            RetryKind::Transient,
        ]
        .into_iter()
        .map(|kind| {
            let budget = Arc::clone(&budget);
            thread::spawn(move || {
                for _ in 0..2 {
                    if budget.try_acquire(kind) {
                        thread::yield_now();
                        budget.release(kind);
                    }
                }
                // A first attempt adds nothing here, and must not push the
                // bucket past its capacity.
                budget.record_request();
            })
        })
        .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(drain(&budget, RetryKind::Transient), 3);
    });
}

/// Concurrent first lookups of one host, in any case, share one bucket. A
/// second bucket would double the host's retry budget.
#[test]
fn retry_budgets_share_one_bucket_per_host() {
    check(|| {
        let budgets = Arc::new(RetryBudgets::new(&small_budget_config()));
        let workers: Vec<_> = ["api.example", "API.example", "Api.Example"]
            .into_iter()
            .map(|host| {
                let budgets = Arc::clone(&budgets);
                thread::spawn(move || budgets.for_host(host))
            })
            .collect();
        let buckets: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
        assert!(Arc::ptr_eq(&buckets[0], &buckets[1]));
        assert!(Arc::ptr_eq(&buckets[1], &buckets[2]));
    });
}

/// Five threads over two tenants, with a cap of two each. No tenant ever holds
/// more than two permits, every permit comes back, and no map entry outlives
/// its last permit.
#[test]
fn bulkhead_caps_each_tenant_and_forgets_idle_ones() {
    check(|| {
        let bulkhead = TenantBulkhead::new(2);
        let held = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let workers: Vec<_> = (0..5)
            .map(|i| {
                let bulkhead = Arc::clone(&bulkhead);
                let held = Arc::clone(&held);
                thread::spawn(move || {
                    let slot = i % 2;
                    let tenant = if slot == 0 { "acme" } else { "globex" };
                    if let Some(permit) = bulkhead.try_acquire(tenant) {
                        let now = held[slot].fetch_add(1, Ordering::SeqCst) + 1;
                        assert!(now <= 2, "{tenant} holds {now} permits, over its cap of 2");
                        thread::yield_now();
                        held[slot].fetch_sub(1, Ordering::SeqCst);
                        drop(permit);
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(bulkhead.in_flight("acme"), 0);
        assert_eq!(bulkhead.in_flight("globex"), 0);
        assert_eq!(bulkhead.tracked_tenants(), 0);
    });
}

/// The same bulkhead across async tasks that hold a permit over an await, the
/// way a request holds a database permit. A refused task retries after a
/// yield, so every task finishes.
#[test]
fn bulkhead_permits_held_across_awaits() {
    check(|| {
        let bulkhead = TenantBulkhead::new(1);
        let held = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicUsize::new(0));
        future::block_on(async {
            let tasks: Vec<_> = (0..3)
                .map(|_| {
                    let bulkhead = Arc::clone(&bulkhead);
                    let held = Arc::clone(&held);
                    let done = Arc::clone(&done);
                    future::spawn(async move {
                        let permit = loop {
                            if let Some(permit) = bulkhead.try_acquire("acme") {
                                break permit;
                            }
                            future::yield_now().await;
                        };
                        assert_eq!(
                            held.fetch_add(1, Ordering::SeqCst),
                            0,
                            "two tasks hold the only permit"
                        );
                        future::yield_now().await;
                        held.fetch_sub(1, Ordering::SeqCst);
                        drop(permit);
                        done.fetch_add(1, Ordering::SeqCst);
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }
        });
        assert_eq!(done.load(Ordering::SeqCst), 3);
        assert_eq!(bulkhead.tracked_tenants(), 0);
    });
}

/// A breaker that opens on the first failure and goes half-open at once.
fn breaker(trials: u64, open_duration: Duration) -> CircuitBreaker {
    CircuitBreaker::new(
        "shuttle",
        CircuitBreakerPolicy {
            failure_ratio_threshold: 0.5,
            minimum_sample_count: 1,
            open_duration,
            half_open_trial_count: trials,
            sample_window: Duration::from_secs(60),
            ..CircuitBreakerPolicy::default()
        },
    )
}

/// Trip `breaker` with one failed call.
async fn trip(breaker: &CircuitBreaker) {
    let tripped = breaker.run(async { Err::<(), ()>(()) }).await;
    assert!(tripped.is_err());
}

/// A half-open breaker lets one trial call run at a time, across concurrent
/// async callers. Each trial here fails, so the breaker opens again and goes
/// half-open at once: it never closes, and no two trials ever overlap.
#[test]
fn circuit_breaker_half_open_runs_one_trial_at_a_time() {
    check(|| {
        let breaker = breaker(1, Duration::ZERO);
        let running = Arc::new(AtomicUsize::new(0));
        let ran = Arc::new(AtomicUsize::new(0));
        future::block_on(async {
            trip(&breaker).await;
            // `open_duration` is zero, so the next call sees `HalfOpen`.
            assert_eq!(breaker.state(), CircuitState::HalfOpen);
            let tasks: Vec<_> = (0..3)
                .map(|_| {
                    let breaker = breaker.clone();
                    let running = Arc::clone(&running);
                    let ran = Arc::clone(&ran);
                    future::spawn(async move {
                        for _ in 0..2 {
                            let _ = breaker
                                .run(async {
                                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                                    assert_eq!(now, 1, "{now} half-open trials ran at once");
                                    ran.fetch_add(1, Ordering::SeqCst);
                                    future::yield_now().await;
                                    running.fetch_sub(1, Ordering::SeqCst);
                                    Err::<(), ()>(())
                                })
                                .await;
                            future::yield_now().await;
                        }
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }
        });
        assert!(ran.load(Ordering::SeqCst) >= 1, "no half-open trial ran");
        assert_ne!(breaker.state(), CircuitState::Closed);
    });
}

/// When the trial succeeds, the breaker closes, whatever the other callers
/// did meanwhile: a refused caller never counts as a trial.
#[test]
fn circuit_breaker_a_successful_trial_closes_it() {
    check(|| {
        let breaker = breaker(1, Duration::ZERO);
        future::block_on(async {
            trip(&breaker).await;
            let tasks: Vec<_> = (0..3)
                .map(|_| {
                    let breaker = breaker.clone();
                    future::spawn(async move {
                        let _ = breaker
                            .run(async {
                                future::yield_now().await;
                                Ok::<(), ()>(())
                            })
                            .await;
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }
        });
        assert_eq!(breaker.state(), CircuitState::Closed);
    });
}

/// Calls admitted while the breaker was closed, and failing after it opened,
/// do not count against the new state: the breaker stays open, and refuses
/// the next call.
#[test]
fn circuit_breaker_stale_failures_do_not_move_an_open_breaker() {
    check(|| {
        let breaker = breaker(1, Duration::from_secs(3_600));
        future::block_on(async {
            let tasks: Vec<_> = (0..3)
                .map(|_| {
                    let breaker = breaker.clone();
                    future::spawn(async move {
                        let _ = breaker
                            .run(async {
                                future::yield_now().await;
                                Err::<(), ()>(())
                            })
                            .await;
                    })
                })
                .collect();
            for task in tasks {
                task.await.unwrap();
            }
        });
        assert_eq!(breaker.state(), CircuitState::Open);
        let refused = future::block_on(breaker.run(async { Ok::<(), ()>(()) }));
        assert!(refused.is_err(), "an open breaker admitted a call");
    });
}

/// The control: Shuttle must find the lost update in a load-then-store
/// counter. If it ever stops finding it, the checks above prove nothing.
#[test]
#[should_panic(expected = "lost update")]
fn shuttle_finds_a_seeded_lost_update() {
    shuttle::check_random(
        || {
            let counter = Arc::new(AtomicUsize::new(0));
            let workers: Vec<_> = (0..2)
                .map(|_| {
                    let counter = Arc::clone(&counter);
                    thread::spawn(move || {
                        let seen = counter.load(Ordering::SeqCst);
                        counter.store(seen + 1, Ordering::SeqCst);
                    })
                })
                .collect();
            for worker in workers {
                worker.join().unwrap();
            }
            assert_eq!(counter.load(Ordering::SeqCst), 2, "lost update");
        },
        iterations(),
    );
}
