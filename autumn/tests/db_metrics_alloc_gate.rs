//! Isolated integration test: allocation gate for recording a DB query sample
//! into the [`MetricsCollector`]'s per-query series.
//!
//! Its own binary for the same reason `config_alloc_gate` is:
//! `allocation-counter` installs a process-wide counting `#[global_allocator]`,
//! a process-wide side effect per CLAUDE.md's isolated-test rules.
//!
//! # What is measured
//!
//! The collector's query map is an intern table: a route's `"METHOD /path VERB"`
//! key is inserted on its first sample and every later sample only updates it.
//! Recording under a key the map already holds must cost a *lookup*, not a
//! fresh owned copy of the key just to probe a map that already has an equal
//! one. And a caller that composes the key from parts (`run_instrumented`'s
//! route plus SQL verb) must compose it on the stack, not into a `String` per
//! query. That is what this gate pins:
//!
//! - **`record_db_query`** with a key it has seen allocates nothing per sample:
//!   the map is probed by `&str` before any owned key is built.
//! - **`run_instrumented`** with a route it has seen allocates nothing per
//!   query: the `"{route} {verb}"` key is formatted into a stack buffer and
//!   handed to the same borrowed probe.
//!
//! The first sample for a key still inserts it (one owned key plus the
//! series' latency ring); the warm-up in [`per_sample`] pays that before the
//! measured window, because the steady state is what this gate pins.
//!
//! Allocations per sample (deterministic across runs). The `before` column is
//! this gate run against the code it replaced: an owned probe key per sample,
//! plus `format!`'s allocate-then-grow for the composed key:
//!
//! | workload                        | before | after |
//! |---------------------------------|--------|-------|
//! | `record_db_query`, seen key     |      1 |     0 |
//! | `run_instrumented`, seen route  |      3 |     0 |
//!
//! A failure here means a refactor put an allocation back on the hit path;
//! re-measure and re-derive rather than raising a ceiling.

use std::hint::black_box;
#[cfg(feature = "db")]
use std::time::Duration;

use autumn_web::middleware::MetricsCollector;

/// Enough samples that a per-sample allocation cannot hide in noise.
const SAMPLES: u64 = 1_000;

/// Warm-up calls before the measured window.
const WARM_UP: u64 = 10;

/// Allocations made by `f` over [`SAMPLES`] calls, after a warm-up that pays
/// every first-sight cost (key insertion, lazy statics, first lock).
fn per_sample(label: &str, mut f: impl FnMut()) -> u64 {
    for _ in 0..WARM_UP {
        f();
    }
    let info = allocation_counter::measure(|| {
        for _ in 0..SAMPLES {
            f();
        }
    });
    let total = info.count_total;
    let per = total / SAMPLES;
    println!("{label}: {total} allocations over {SAMPLES} samples ({per}/sample)");
    // Every sample takes the same path, so the total is an exact multiple. A
    // remainder means some samples allocate and others do not, which a
    // per-sample quotient alone would round away.
    assert_eq!(
        total % SAMPLES,
        0,
        "{label}: {total} allocations over {SAMPLES} samples is not a whole number per sample"
    );
    per
}

#[test]
fn db_query_with_a_seen_key_records_without_allocating() {
    let collector = MetricsCollector::new();
    let allocs = per_sample("db query, seen key", || {
        collector.record_db_query("GET /users/{id} SELECT", black_box(5));
    });
    assert_eq!(
        allocs, 0,
        "a DB query key the collector holds must be probed by borrow"
    );
    // Every sample, warm-up included, landed in the one series.
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.db_queries.len(), 1);
    assert_eq!(
        snapshot.db_queries["GET /users/{id} SELECT"].count,
        WARM_UP + SAMPLES
    );
}

#[cfg(feature = "db")]
#[test]
fn instrumented_query_with_a_seen_key_records_without_allocating() {
    use futures::FutureExt as _;

    let collector = MetricsCollector::new();
    let allocs = per_sample("run_instrumented, seen key", || {
        // The query resolves immediately, so no database is needed. The
        // threshold keeps the slow-query `warn!` callsite cold.
        let outcome = autumn_web::db::run_instrumented(
            "SELECT id FROM users WHERE id = $1",
            "GET /users/{id}",
            Duration::from_secs(3_600),
            &collector,
            || std::future::ready(Ok(())),
        )
        .now_or_never();
        black_box(&outcome);
    });
    assert_eq!(
        allocs, 0,
        "an instrumented query under a seen key must not allocate"
    );
    // The key is still `"{route} {verb}"`, byte for byte.
    let snapshot = collector.snapshot();
    assert_eq!(snapshot.db_queries.len(), 1);
    assert_eq!(
        snapshot.db_queries["GET /users/{id} SELECT"].count,
        WARM_UP + SAMPLES
    );
}
