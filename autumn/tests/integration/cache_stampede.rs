//! Integration tests for read-through cache stampede protection (issue #1204).
//!
//! These exercise the public API (`autumn_web::cache::get_or_compute` /
//! `get_or_compute_with`) against the in-process `MokaCache` backend, proving
//! the single-flight, failure-semantics, and stale-while-revalidate
//! acceptance criteria. Redis-specific (cross-replica) coverage lives in
//! `autumn-cache-redis/src/lib.rs`.

#![cfg(feature = "cache-moka")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use autumn_web::cache::MokaCache;
use autumn_web::cache::{
    Cache, CacheFillError, GetOrComputeOptions, get_cached, get_or_compute, get_or_compute_with,
    read_through_metrics,
};

/// Serializes tests that touch `read_through_metrics()` (a process-wide
/// singleton), since `cargo test` runs tests in parallel by default and
/// several of these tests assert *exact* metric deltas. A `tokio::sync::Mutex`
/// is used (rather than `std::sync::Mutex`) because the guard is held across
/// `.await` points for the whole test body.
static METRICS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fresh_cache() -> Arc<dyn Cache> {
    Arc::new(MokaCache::new(1_000, None))
}

fn unique_key(name: &str) -> String {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("cache-stampede-test:{name}:{n}")
}

#[tokio::test(flavor = "multi_thread")]
async fn miss_fills_then_hits() {
    let _guard = METRICS_LOCK.lock().await;
    let cache = fresh_cache();
    let key = unique_key("miss_fills_then_hits");
    let fill_count = Arc::new(AtomicUsize::new(0));

    let before = read_through_metrics().snapshot();

    let fc = fill_count.clone();
    let v: i32 = get_or_compute(&cache, &key, None, || async move {
        fc.fetch_add(1, Ordering::SeqCst);
        Ok::<i32, String>(42)
    })
    .await
    .unwrap();
    assert_eq!(v, 42);
    assert_eq!(fill_count.load(Ordering::SeqCst), 1);

    let fc = fill_count.clone();
    let v: i32 = get_or_compute(&cache, &key, None, || async move {
        fc.fetch_add(1, Ordering::SeqCst);
        Ok::<i32, String>(99) // should never run: cache should hit
    })
    .await
    .unwrap();
    assert_eq!(v, 42, "second call must hit the cache, not refill");
    assert_eq!(
        fill_count.load(Ordering::SeqCst),
        1,
        "fill must not run again on a hit"
    );

    let after = read_through_metrics().snapshot();
    assert_eq!(after.fills - before.fills, 1);
    assert_eq!(after.hits - before.hits, 1);
    assert!(after.misses - before.misses >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_misses_run_fill_once() {
    // The issue's falsifiable success metric: K concurrent requests at an
    // expired/missing key produce exactly 1 fill and K-1 coalesced waits.
    //
    // Rather than guess a sleep duration long enough to outlast scheduler
    // jitter on shared/oversubscribed CI hardware, the fill closure blocks
    // until every contender has actually reached `get_or_compute` (tracked by
    // `started`). That makes the test deterministic: whichever task becomes
    // leader always waits for all K contenders before finishing its fill, no
    // matter how delayed some of them are to get scheduled.
    const K: usize = 16;

    let _guard = METRICS_LOCK.lock().await;
    let cache = fresh_cache();
    let key = unique_key("concurrent_misses_run_fill_once");
    let fill_count = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));

    let before = read_through_metrics().snapshot();

    let mut handles = Vec::with_capacity(K);
    for _ in 0..K {
        let cache = cache.clone();
        let key = key.clone();
        let fc = fill_count.clone();
        let started = started.clone();
        handles.push(tokio::spawn(async move {
            started.fetch_add(1, Ordering::SeqCst);
            get_or_compute::<i32, String, _, _>(&cache, &key, None, || async move {
                fc.fetch_add(1, Ordering::SeqCst);
                while started.load(Ordering::SeqCst) < K {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                // Small extra margin for the last-registered contender to
                // finish its (synchronous, no-await) claim_role call.
                tokio::time::sleep(Duration::from_millis(50)).await;
                Ok(7)
            })
            .await
        }));
    }

    for h in handles {
        let v = tokio::time::timeout(Duration::from_secs(30), h)
            .await
            .expect("all contenders must resolve well within 30s")
            .unwrap()
            .unwrap();
        assert_eq!(v, 7);
    }

    assert_eq!(
        fill_count.load(Ordering::SeqCst),
        1,
        "exactly one fill must run for {K} concurrent misses on the same key"
    );

    let after = read_through_metrics().snapshot();
    assert_eq!(after.fills - before.fills, 1);
    assert_eq!(
        after.coalesced_waits - before.coalesced_waits,
        (K - 1) as u64
    );
    assert!(after.misses - before.misses >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn fill_error_propagates_and_does_not_poison() {
    const WAITERS: usize = 4;

    let _guard = METRICS_LOCK.lock().await;
    let cache = fresh_cache();
    let key = unique_key("fill_error_propagates_and_does_not_poison");
    let attempt = Arc::new(AtomicUsize::new(0));

    let before = read_through_metrics().snapshot();

    let started = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(WAITERS);
    for _ in 0..WAITERS {
        let cache = cache.clone();
        let key = key.clone();
        let attempt = attempt.clone();
        let started = started.clone();
        handles.push(tokio::spawn(async move {
            started.fetch_add(1, Ordering::SeqCst);
            get_or_compute::<i32, String, _, _>(&cache, &key, None, || async move {
                attempt.fetch_add(1, Ordering::SeqCst);
                while started.load(Ordering::SeqCst) < WAITERS {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                Err("boom".to_string())
            })
            .await
        }));
    }

    let mut saw_typed_fill = false;
    let mut saw_fill_failed = false;
    for h in handles {
        match h.await.unwrap() {
            Err(CacheFillError::Fill(e)) => {
                assert_eq!(e, "boom");
                saw_typed_fill = true;
            }
            Err(CacheFillError::FillFailed(msg)) => {
                assert!(
                    msg.contains("boom"),
                    "waiter error should mention 'boom': {msg}"
                );
                saw_fill_failed = true;
            }
            Ok(_) => panic!("a failing fill must never return Ok"),
        }
    }
    assert!(
        saw_typed_fill,
        "the leader must get a typed CacheFillError::Fill"
    );
    // With WAITERS=4 concurrent callers there should be at least one coalesced waiter.
    assert!(
        saw_fill_failed,
        "at least one waiter must see CacheFillError::FillFailed"
    );

    assert!(
        get_cached::<i32>(&*cache, &key).is_none(),
        "a failed fill must not write anything to the cache"
    );

    let after = read_through_metrics().snapshot();
    assert!(after.fill_failures - before.fill_failures >= 1);

    // The key is not poisoned: the next caller retries and can succeed. Use a
    // fresh counter for the retry's own closure so we can prove *this* call
    // actually ran a fill rather than serving a (nonexistent) cached value.
    let retry_ran = Arc::new(AtomicUsize::new(0));
    let retry_ran2 = retry_ran.clone();
    let v: i32 = get_or_compute(&cache, &key, None, || async move {
        retry_ran2.fetch_add(1, Ordering::SeqCst);
        Ok::<i32, String>(5)
    })
    .await
    .unwrap();
    assert_eq!(v, 5);
    assert_eq!(
        retry_ran.load(Ordering::SeqCst),
        1,
        "retry after failure must run its fill closure exactly once"
    );
    assert!(
        attempt.load(Ordering::SeqCst) >= 1,
        "the failing round must have run at least once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_leader_does_not_deadlock_waiters() {
    // read_through_metrics() is a process-wide singleton; hold the same lock
    // other tests use so concurrent metric increments don't race (this test
    // doesn't assert on metrics itself, but must not pollute others' deltas
    // while it runs).
    let _guard = METRICS_LOCK.lock().await;
    let cache = fresh_cache();
    let key = unique_key("cancelled_leader_does_not_deadlock_waiters");

    let leader_key = key.clone();
    let leader_cache = cache.clone();
    let leader = tokio::spawn(async move {
        get_or_compute::<i32, String, _, _>(&leader_cache, &leader_key, None, || async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(1)
        })
        .await
    });

    // Give the leader a moment to register as the in-flight leader.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let waiter_key = key.clone();
    let waiter_cache = cache.clone();
    let waiter =
        tokio::spawn(async move {
            get_or_compute::<i32, String, _, _>(&waiter_cache, &waiter_key, None, || async move {
                Ok(2)
            })
            .await
        });

    // Abort the stuck leader; the waiter must recover (re-contend for
    // leadership) instead of hanging forever.
    tokio::time::sleep(Duration::from_millis(50)).await;
    leader.abort();

    let result = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("waiter must not deadlock after the leader is cancelled")
        .unwrap();
    assert_eq!(result.unwrap(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn different_keys_do_not_coalesce() {
    let _guard = METRICS_LOCK.lock().await;
    let cache = fresh_cache();
    let key_a = unique_key("different_keys_a");
    let key_b = unique_key("different_keys_b");
    let fill_count = Arc::new(AtomicUsize::new(0));

    let fc_a = fill_count.clone();
    let cache_a = cache.clone();
    let a = tokio::spawn(async move {
        get_or_compute::<i32, String, _, _>(&cache_a, &key_a, None, || async move {
            fc_a.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok(1)
        })
        .await
    });

    let fc_b = fill_count.clone();
    let cache_b = cache.clone();
    let b = tokio::spawn(async move {
        get_or_compute::<i32, String, _, _>(&cache_b, &key_b, None, || async move {
            fc_b.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            Ok(2)
        })
        .await
    });

    assert_eq!(a.await.unwrap().unwrap(), 1);
    assert_eq!(b.await.unwrap().unwrap(), 2);
    assert_eq!(
        fill_count.load(Ordering::SeqCst),
        2,
        "distinct keys must each fill"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn swr_serves_stale_and_refreshes_in_background() {
    // Floor-not-ceiling hang-guard for the refresh-notify wait below.
    // Deliberately generous: it must never trip on a slow/oversubscribed runner
    // — only on a genuine never-fires hang. A real windows-latest run starved
    // the detached refresh past 30s (PR #1764), so this is set well above any
    // plausible scheduling-starvation window. (The publish-visibility poll
    // further below is bounded by attempt count, not wall-clock time — see
    // #1809 — so it cannot elapse merely because the suite runs slow.)
    const HANG_GUARD: Duration = Duration::from_secs(120);

    let _guard = METRICS_LOCK.lock().await;
    let cache = fresh_cache();
    let key = unique_key("swr_serves_stale_and_refreshes_in_background");
    let fill_count = Arc::new(AtomicUsize::new(0));

    let before = read_through_metrics().snapshot();

    let opts = GetOrComputeOptions::new()
        .ttl(Duration::from_millis(50))
        .stale_while_revalidate(Duration::from_secs(10));

    let fc = fill_count.clone();
    let v: String = get_or_compute_with(&cache, &key, opts.clone(), move || async move {
        fc.fetch_add(1, Ordering::SeqCst);
        Ok::<String, String>("v1".to_string())
    })
    .await
    .unwrap();
    assert_eq!(v, "v1");

    // Let the freshness TTL elapse so the value is stale (but still within
    // the grace period).
    tokio::time::sleep(Duration::from_millis(80)).await;

    // A test-controlled gate the background refresh's fill closure blocks on
    // instead of a fixed `sleep`, so the refresh completes only when the test
    // explicitly releases it. This is what makes the stale-serve non-blocking
    // property below provable with ZERO wall-clock dependence (the former
    // `elapsed < 150ms` ceiling flaked under windows-latest scheduler
    // starvation — an OS deschedule between `Instant::now()` and the future
    // resolving was wrongly charged to `elapsed`; see #1809). Because the fill
    // cannot finish until released, if the foreground stale-serve had waited on
    // the refresh the call would hang forever — caught by HANG_GUARD as a hard
    // failure, never a flaky pass — so the mere fact it returns the stale value
    // while the fill is still gated proves it did not block on the refresh.
    let refresh_gate = Arc::new(tokio::sync::Notify::new());

    // A deterministic signal the background refresh's fill closure fires the
    // moment it finishes computing the new value (after the gate opens). Waiting
    // on this — rather than a fixed wall-clock timeout — absorbs however long a
    // slow/loaded runner takes to schedule and run the detached background task.
    let refresh_done = Arc::new(tokio::sync::Notify::new());

    let fc = fill_count.clone();
    let gate = refresh_gate.clone();
    let done = refresh_done.clone();
    let v: String = get_or_compute_with(&cache, &key, opts.clone(), move || async move {
        fc.fetch_add(1, Ordering::SeqCst);
        // Block until the test releases the gate. Replaces the former
        // `sleep(200ms)` + `elapsed < 150ms` wall-clock ceiling: the stale-serve
        // property is now proven structurally by the fact this fill cannot
        // complete until released, so a starved scheduler can only DELAY the
        // refresh, never turn the assertion below into a false failure.
        gate.notified().await;
        let out = Ok::<String, String>("v2".to_string());
        // Publication (`insert_cached`) happens synchronously right after this
        // closure returns; signal now that the compute is done.
        done.notify_one();
        out
    })
    .await
    .unwrap();
    assert_eq!(
        v, "v1",
        "a stale-but-in-grace value must be served immediately without waiting on the \
         (still-gated) background refresh"
    );

    // Strengthen the proof: while the refresh fill is still gated it cannot have
    // published "v2", so a second read also serves the stale "v1". The in-flight
    // refresh holds single-flight leadership, so this read neither runs a second
    // fill (`unexpected-refill` is dropped unrun) nor blocks on it — it is fully
    // deterministic, with no timing assumption.
    let fc = fill_count.clone();
    let still_stale: String = get_or_compute_with(&cache, &key, opts.clone(), move || async move {
        fc.fetch_add(1, Ordering::SeqCst);
        Ok::<String, String>("unexpected-refill".to_string())
    })
    .await
    .unwrap();
    assert_eq!(
        still_stale, "v1",
        "the gated background refresh must not have published a new value yet"
    );

    // Release the gate so the background refresh can finish computing and
    // publish. `notify_one` stores a permit if the detached refresh task has not
    // yet parked on `notified()`, so there is no lost-wakeup race regardless of
    // which task the scheduler runs first.
    refresh_gate.notify_one();

    // Wait deterministically for the background refresh to finish computing
    // (now that the gate is open) — no wall-clock ceiling on the load-sensitive
    // scheduling of the detached task. (`notify_one` stores a permit if it fires
    // before this await, so there is no lost-wakeup even when the background task
    // wins the race.)
    //
    // The timeout here is a generous hang-guard, mirroring the bound on the
    // publish loop below: it exists only to convert a genuine never-fires hang
    // (e.g. the detached refresh task is dropped when the global refresh
    // semaphore is exhausted, or a regression stops it ever reaching the fill
    // closure) into a clean test failure instead of dangling until the CI
    // runner's global timeout. It is deliberately NOT a tight, schedule-
    // sensitive value, so it never reintroduces load-dependent flakiness.
    tokio::time::timeout(HANG_GUARD, refresh_done.notified())
        .await
        .expect("background refresh never signalled completion (task dropped or never started)");

    // Confirm the refreshed value becomes visible, by POLLING (re-read, short
    // sleep, repeat) rather than reading once — never collapse this back to a
    // single-shot read or a tight ceiling.
    //
    // WHY A POLL IS REQUIRED: the `Notify` above is fired from *inside* the
    // fill closure, but publication (`finish_fill` -> `insert_cached`) runs in
    // product code *after* the closure returns, on the detached refresh task —
    // concurrently with this woken test task. So the instant we wake on the
    // notify, "v2" may not yet be visible to a fresh read; and until the stale
    // "v1" envelope ages past `ttl + stale_while_revalidate` (~10s) each read
    // instead blocks as a single-flight waiter on that same detached task's
    // channel. Either way, test progress is coupled to the detached refresh
    // being *scheduled* to run its publish — which a heavily oversubscribed CI
    // runner can starve for many seconds (a real windows-latest run starved it
    // past 30s; see PR #1764).
    //
    // WHY THE BOUND IS ON ATTEMPTS, NOT WALL-CLOCK TIME: a fixed
    // `tokio::time::timeout` deadline here is timing-fragile — on an
    // oversubscribed runner the ~35-min `autumn-web` integration suite could
    // let the deadline elapse before the refresh published, tripping
    // `Elapsed(())` even though nothing is wrong (#1809). Instead we cap the
    // number of *read attempts*: under load each attempt simply takes longer,
    // but the loop still gets its full quota of retries before giving up, so it
    // can never fail merely because the runner is slow. It still converts a
    // genuine "publish never happens" regression (dropped/hung refresh task)
    // into a clean failure instead of dangling until the job-level timeout. The
    // cap is deliberately generous: under any sane load this converges in a
    // handful of iterations (the refresh computes in 200ms), so it is not
    // schedule-sensitive; do not swap it back for a wall-clock deadline. 1,000
    // attempts x 25ms is ~25s worst case, which fails a genuine "never
    // publishes" regression fast while still tolerating a slow runner.
    //
    // The attempt cap is wrapped in a generous 120s `HANG_GUARD` backstop. The
    // cap remains the primary load-scaling bound, but if a refresh signals
    // completion (`refresh_done`) yet never publishes, then once the stale
    // grace expires every poll read falls through to the single-flight WAITER
    // path and parks forever — consuming zero attempts. The 120s timeout
    // converts that parked-waiter hang into a clean `Elapsed` failure instead
    // of dangling until the job-level timeout.
    let refreshed = tokio::time::timeout(HANG_GUARD, async {
        let mut refreshed = None;
        for _ in 0..1_000 {
            let fc = fill_count.clone();
            let v: String = get_or_compute_with(&cache, &key, opts.clone(), move || async move {
                fc.fetch_add(1, Ordering::SeqCst);
                Ok::<String, String>("unexpected-refill".to_string())
            })
            .await
            .unwrap();
            if v == "v2" {
                refreshed = Some(v);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        refreshed
    })
    .await
    .expect(
        "publish-visibility poll exceeded the hang guard: a read parked as a single-flight \
         waiter on a refresh that signalled completion but never published, so the attempt \
         cap never advanced",
    )
    .expect("background refresh must publish the new value after it finishes computing");
    assert_eq!(refreshed, "v2");

    let after = read_through_metrics().snapshot();
    assert!(after.stale_serves - before.stale_serves >= 1);
    assert!(fill_count.load(Ordering::SeqCst) >= 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn swr_only_one_background_refresh() {
    const N: usize = 8;

    let _guard = METRICS_LOCK.lock().await;
    let cache = fresh_cache();
    let key = unique_key("swr_only_one_background_refresh");
    let fill_count = Arc::new(AtomicUsize::new(0));

    let opts = GetOrComputeOptions::new()
        .ttl(Duration::from_millis(50))
        .stale_while_revalidate(Duration::from_secs(10));

    let fc = fill_count.clone();
    get_or_compute_with(&cache, &key, opts.clone(), move || async move {
        fc.fetch_add(1, Ordering::SeqCst);
        Ok::<String, String>("v1".to_string())
    })
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(80)).await;

    let mut handles = Vec::with_capacity(N);
    for _ in 0..N {
        let cache = cache.clone();
        let key = key.clone();
        let opts = opts.clone();
        let fc = fill_count.clone();
        handles.push(tokio::spawn(async move {
            get_or_compute_with(&cache, &key, opts, move || async move {
                fc.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(150)).await;
                Ok::<String, String>("v2".to_string())
            })
            .await
        }));
    }

    for h in handles {
        let v: String = h.await.unwrap().unwrap();
        assert_eq!(
            v, "v1",
            "all concurrent stale reads must return the stale value fast"
        );
    }

    // Give the single background refresh time to land, then confirm exactly
    // one refresh fill ran (1 initial fill + 1 refresh = 2 total).
    tokio::time::timeout(Duration::from_secs(2), async {
        while fill_count.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background refresh must complete");

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        fill_count.load(Ordering::SeqCst),
        2,
        "only one background refresh should run despite N concurrent stale reads"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn swr_without_ttl_never_goes_stale() {
    let _guard = METRICS_LOCK.lock().await;
    let cache = fresh_cache();
    let key = unique_key("swr_without_ttl_never_goes_stale");
    let fill_count = Arc::new(AtomicUsize::new(0));

    // SWR enabled but no `.ttl(...)`: per its own doc comment, `ttl: None`
    // means "no expiry", so the value must stay fresh forever rather than
    // going stale (and re-triggering a background refresh) on every read
    // after the very first.
    let opts = GetOrComputeOptions::new().stale_while_revalidate(Duration::from_secs(10));

    let fc = fill_count.clone();
    let v: String = get_or_compute_with(&cache, &key, opts.clone(), move || async move {
        fc.fetch_add(1, Ordering::SeqCst);
        Ok::<String, String>("v1".to_string())
    })
    .await
    .unwrap();
    assert_eq!(v, "v1");

    // Real time elapses. With the bug, any elapsed time makes the entry
    // stale, since `fresh_until` was stamped as "now" at write time.
    tokio::time::sleep(Duration::from_millis(50)).await;

    for _ in 0..5 {
        let fc = fill_count.clone();
        let v: String = get_or_compute_with(&cache, &key, opts.clone(), move || async move {
            fc.fetch_add(1, Ordering::SeqCst);
            Ok::<String, String>("unexpected-refill".to_string())
        })
        .await
        .unwrap();
        assert_eq!(v, "v1", "a value with no TTL must never appear stale");
    }

    // Give any (incorrectly) spawned background refresh a chance to run.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        fill_count.load(Ordering::SeqCst),
        1,
        "no TTL + SWR must mean 'never stale': only the first fill should ever run"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn swr_past_grace_window_becomes_cold_miss() {
    let _guard = METRICS_LOCK.lock().await;
    let cache = fresh_cache();
    let key = unique_key("swr_past_grace_window_becomes_cold_miss");
    let fill_count = Arc::new(AtomicUsize::new(0));

    let opts = GetOrComputeOptions::new()
        .ttl(Duration::from_millis(30))
        .stale_while_revalidate(Duration::from_millis(30));

    let fc = fill_count.clone();
    let v: String = get_or_compute_with(&cache, &key, opts.clone(), move || async move {
        fc.fetch_add(1, Ordering::SeqCst);
        Ok::<String, String>("v1".to_string())
    })
    .await
    .unwrap();
    assert_eq!(v, "v1");

    // Elapse past both `ttl` (30ms) and `grace` (30ms): per the docs ("Once
    // past ttl + grace, the key is treated as a cold miss again"), the entry
    // must no longer be served as stale-but-usable data — the in-process
    // Moka backend never physically evicts it on its own (its own per-cache
    // TTL here is `None`), so this can't rely on eviction to happen.
    tokio::time::sleep(Duration::from_millis(120)).await;

    let fc = fill_count.clone();
    let v: String = get_or_compute_with(&cache, &key, opts.clone(), move || async move {
        fc.fetch_add(1, Ordering::SeqCst);
        Ok::<String, String>("v2".to_string())
    })
    .await
    .unwrap();
    assert_eq!(
        v, "v2",
        "past ttl + grace the caller must get a fresh fill, not the ancient stale value"
    );
    assert_eq!(
        fill_count.load(Ordering::SeqCst),
        2,
        "exactly one fresh fill should run for the cold-miss read"
    );
}

/// A minimal `Cache` implementation that only ever stores `RawCacheBytes`
/// (mirroring how a serializing, cross-process backend like Redis behaves),
/// proving `get_or_compute` works over the serde slow path too.
#[derive(Default)]
struct RawBytesCache {
    inner: std::sync::Mutex<std::collections::HashMap<String, autumn_web::cache::RawCacheBytes>>,
}

impl Cache for RawBytesCache {
    fn get_value(&self, key: &str) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .map(|raw| Arc::new(raw.clone()) as Arc<dyn std::any::Any + Send + Sync>)
    }

    fn insert_value(&self, _key: &str, _value: Arc<dyn std::any::Any + Send + Sync>) {
        // Simulate a backend that only accepts the serialized path.
    }

    fn invalidate(&self, key: &str) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }

    fn clear(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    fn insert_raw_bytes(&self, key: &str, bytes: Vec<u8>, _ttl: Option<Duration>) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), autumn_web::cache::RawCacheBytes(bytes));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn works_through_arc_dyn_cache_raw_bytes_backend() {
    let _guard = METRICS_LOCK.lock().await;
    let cache: Arc<dyn Cache> = Arc::new(RawBytesCache::default());
    let key = unique_key("works_through_arc_dyn_cache_raw_bytes_backend");

    let v: String = get_or_compute(&cache, &key, None, || async move {
        Ok::<String, String>("hello".to_string())
    })
    .await
    .unwrap();
    assert_eq!(v, "hello");

    // Second call must hit via the RawCacheBytes/serde_json slow path.
    let v: String = get_or_compute(&cache, &key, None, || async move {
        Ok::<String, String>("should-not-run".to_string())
    })
    .await
    .unwrap();
    assert_eq!(v, "hello");
}
