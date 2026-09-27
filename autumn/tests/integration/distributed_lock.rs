//! Database-level integration tests for the app-facing distributed lock
//! (`autumn_web::lock::Lock`, issue #1387).
//!
//! These tests exercise real `PostgreSQL` advisory-lock semantics and therefore
//! **require Docker** (testcontainers); they are `#[ignore]`d by default, in the
//! same style as `repository_find_in_batches.rs`. The success metric from the
//! issue — "with N concurrent nodes contending on the same lock name the guarded
//! section executes on exactly one node at a time (no overlap, no leaked lock
//! after a panicking section)" — is covered by
//! `exactly_one_node_holds_at_a_time` and `panicking_section_does_not_leak`.

#![cfg(feature = "db")]

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use autumn_web::lock::{Lock, LockError};
use diesel_async::AsyncPgConnection;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::pooled_connection::deadpool::Pool;
use testcontainers::runners::AsyncRunner;
use testcontainers_modules::postgres::Postgres;

async fn setup_pool() -> (
    Pool<AsyncPgConnection>,
    testcontainers::ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .start()
        .await
        .expect("failed to start postgres container");

    let host = container.get_host().await.expect("host");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    // Enough connections for several concurrently-held locks plus contenders.
    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(&url);
    let pool = Pool::builder(manager).max_size(16).build().expect("pool");
    (pool, container)
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn try_lock_is_exclusive_and_reacquirable() {
    let (pool, _container) = setup_pool().await;
    let lock = Lock::new(pool.clone(), "test-exclusive");

    let guard = lock
        .try_lock()
        .await
        .expect("first try_lock should not error")
        .expect("first try_lock should acquire");

    // A second, independent handle on the same name must not acquire.
    let other = Lock::new(pool.clone(), "test-exclusive");
    assert!(
        other
            .try_lock()
            .await
            .expect("second try_lock should not error")
            .is_none(),
        "the lock must stay exclusive while held"
    );

    guard.release().await.expect("release should succeed");

    // After release the lock is available again.
    let reacquired = other.try_lock().await.expect("reacquire should not error");
    assert!(
        reacquired.is_some(),
        "the lock must be available again after release"
    );
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn distinct_names_do_not_contend() {
    let (pool, _container) = setup_pool().await;
    let a = Lock::new(pool.clone(), "test-name-a");
    let b = Lock::new(pool.clone(), "test-name-b");

    let ga = a.try_lock().await.expect("a").expect("a acquires");
    let gb = b
        .try_lock()
        .await
        .expect("b")
        .expect("b acquires despite a");

    ga.release().await.expect("release a");
    gb.release().await.expect("release b");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn with_auto_releases_after_section() {
    let (pool, _container) = setup_pool().await;
    let lock = Lock::new(pool.clone(), "test-with-release");

    let out = lock
        .with(|| async { 21 * 2 })
        .await
        .expect("with should acquire");
    assert_eq!(out, 42);

    // The lock must be free immediately after `with` returns.
    let again = lock.try_lock().await.expect("post-with try_lock");
    assert!(again.is_some(), "with must release the lock on return");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn try_with_returns_none_when_held() {
    let (pool, _container) = setup_pool().await;
    let lock = Lock::new(pool.clone(), "test-try-with");

    let held = lock.lock().await.expect("blocking acquire");

    let ran = Arc::new(AtomicUsize::new(0));
    let ran_clone = Arc::clone(&ran);
    let result: Option<()> = lock
        .try_with(|| async move {
            ran_clone.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .expect("try_with should not error");
    assert!(result.is_none(), "try_with must not run while lock is held");
    assert_eq!(ran.load(Ordering::SeqCst), 0, "closure must not have run");

    held.release().await.expect("release");
}

#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn lock_timeout_expires_when_held() {
    let (pool, _container) = setup_pool().await;
    let lock =
        Lock::new(pool.clone(), "test-timeout").with_poll_interval(Duration::from_millis(20));

    let held = lock.lock().await.expect("first acquire");

    let err = lock
        .lock_timeout(Duration::from_millis(120))
        .await
        .expect_err("should time out while lock is held");
    assert!(
        matches!(err, LockError::Timeout { .. }),
        "expected a Timeout error, got {err:?}"
    );

    held.release().await.expect("release");

    // Once released, the timed acquire succeeds.
    let g = lock
        .lock_timeout(Duration::from_millis(500))
        .await
        .expect("acquire after release");
    g.release().await.expect("cleanup release");
}

/// Success metric: N concurrent nodes contending on the same lock name execute
/// the guarded section one at a time — the in-section counter is never > 1.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn exactly_one_node_holds_at_a_time() {
    const NODES: usize = 8;

    let (pool, _container) = setup_pool().await;

    let in_section = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let ran = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..NODES {
        let lock = Lock::new(pool.clone(), "test-mutual-exclusion");
        let in_section = Arc::clone(&in_section);
        let max_seen = Arc::clone(&max_seen);
        let ran = Arc::clone(&ran);
        handles.push(tokio::spawn(async move {
            lock.with(|| async move {
                let now = in_section.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                // Hold the section briefly so overlap would be observable.
                tokio::time::sleep(Duration::from_millis(25)).await;
                in_section.fetch_sub(1, Ordering::SeqCst);
                ran.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .expect("each node should eventually acquire");
        }));
    }

    for h in handles {
        h.await.expect("node task should not panic");
    }

    assert_eq!(
        ran.load(Ordering::SeqCst),
        NODES,
        "every node must run once"
    );
    assert_eq!(
        max_seen.load(Ordering::SeqCst),
        1,
        "at most one node may be in the guarded section at a time"
    );
}

/// Cancellation safety (P1, issue #1387): if an acquire future is dropped
/// *mid-acquire* — here a blocking `lock()` cancelled by a surrounding
/// `tokio::time::timeout` while it is still contending for a held lock — the
/// acquiring connection must be force-closed, never recycled into the pool
/// while it could be holding (or about to hold) the advisory lock. We prove the
/// lock did not leak by acquiring it fresh, from a new connection, after the
/// cancelled acquire and after the original holder releases.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cancelled_acquire_does_not_leak_lock() {
    let (pool, _container) = setup_pool().await;

    // Hold the lock so the contender below must block inside `pg_advisory_lock`.
    let holder = Lock::new(pool.clone(), "test-cancel-safety");
    let held = holder.lock().await.expect("holder should acquire");

    // Cancel a blocking acquire while it is mid-flight (still waiting on the
    // held lock). Dropping the timed-out future runs the internal acquire
    // guard's `Drop`, which force-closes that session.
    let contender = Lock::new(pool.clone(), "test-cancel-safety");
    let timed_out = tokio::time::timeout(Duration::from_millis(75), contender.lock()).await;
    assert!(
        timed_out.is_err(),
        "the contender's acquire should have been cancelled by the timeout"
    );

    // Release the original holder; the lock must now be genuinely free — the
    // cancelled contender must not have leaked a lock-bearing connection back
    // into the pool.
    held.release().await.expect("holder release should succeed");

    // A fresh handle (new pooled connection) must be able to acquire it,
    // proving the session did not leak the lock into the pool.
    let reacquired = Lock::new(pool.clone(), "test-cancel-safety")
        .try_lock()
        .await
        .expect("post-cancellation try_lock should not error");
    assert!(
        reacquired.is_some(),
        "a cancelled mid-acquire future must not leak the distributed lock"
    );
    reacquired
        .expect("guard present")
        .release()
        .await
        .expect("cleanup release");
}

/// Cancellation safety on the *release* path (P1, issue #1387): if a
/// `release()` future is dropped while `pg_advisory_unlock` is still in-flight
/// — here by polling it exactly once by hand and dropping it without polling
/// again — the still-checked-out session must be force-closed rather than
/// recycled into the pool while it could still be holding the advisory lock. We
/// prove the lock did not leak by re-acquiring it from a fresh connection.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn cancelled_release_does_not_leak_lock() {
    let (pool, _container) = setup_pool().await;

    let lock = Lock::new(pool.clone(), "test-cancel-release");
    let guard = lock.lock().await.expect("acquire should succeed");

    // Cancel the release while the unlock query is still awaiting. This used
    // to race a `Duration::ZERO` `tokio::time::timeout` against the real
    // `pg_advisory_unlock` round-trip on the theory that a zero-duration
    // timer always wins — but `Timeout::poll` polls the wrapped future
    // *before* checking the timer, so whenever the query happened to
    // resolve within that same first poll (fast local testcontainer,
    // scheduler already ran the connection's background driver task) the
    // timeout never fired and the future was never actually cancelled
    // mid-flight. Measured at 1/30 same-commit reruns (CI issue: this test
    // was `--skip`-listed in `ci.yml` for exactly this flake, with no
    // tracking issue).
    //
    // Poll the future by hand instead: this has no timing dependency at
    // all. Asserting `Poll::Pending` on the very first poll *is* the proof
    // the query had not completed yet (a real network round-trip cannot
    // resolve synchronously on its first poll), and dropping the
    // still-pinned future then cancels it deterministically at exactly that
    // point — no race with anything.
    let mut release_future = Box::pin(guard.release());
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    assert!(
        release_future.as_mut().poll(&mut cx).is_pending(),
        "the first poll of release() should not resolve synchronously — \
         pg_advisory_unlock always requires a real network round-trip"
    );
    // `release_future` is a `Pin<Box<_>>`, so this actually drops the
    // suspended future in place (a stack-pinned `Pin<&mut _>` from
    // `futures::pin_mut!` would NOT: dropping the reference is a no-op and
    // the owned future would live on, undropped, until the enclosing scope
    // ends — silently defeating the whole point of this test).
    drop(release_future);

    // The lock must be genuinely free: a fresh handle on a new pooled
    // connection must be able to acquire it.
    let reacquired = Lock::new(pool.clone(), "test-cancel-release")
        .try_lock()
        .await
        .expect("post-cancellation try_lock should not error");
    assert!(
        reacquired.is_some(),
        "a cancelled mid-release future must not leak the distributed lock"
    );
    reacquired
        .expect("guard present")
        .release()
        .await
        .expect("cleanup release");
}

/// A panicking guarded section must not leak the lock: the next contender can
/// still acquire it.
#[tokio::test]
#[ignore = "requires Docker (testcontainers)"]
async fn panicking_section_does_not_leak() {
    let (pool, _container) = setup_pool().await;
    let lock = Lock::new(pool.clone(), "test-panic-release");

    let lock_for_panic = lock.clone();
    let joined = tokio::spawn(async move {
        lock_for_panic
            .with(|| async {
                panic!("boom inside the guarded section");
            })
            .await
            .expect("acquire before panic")
    })
    .await;
    assert!(joined.is_err(), "the guarded section should have panicked");

    // Despite the panic, the lock must be free again.
    let guard = lock
        .try_lock()
        .await
        .expect("post-panic try_lock should not error");
    assert!(
        guard.is_some(),
        "a panic in the guarded section must not leak the lock"
    );
}
