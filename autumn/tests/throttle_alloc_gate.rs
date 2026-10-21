//! Isolated integration test: allocation gate for the `#[throttle]` per-route
//! check (`autumn_web::security::__check_throttle`) on a warm route.
//!
//! Its own binary for the same reason `config_alloc_gate` is:
//! `allocation-counter` installs a process-wide counting `#[global_allocator]`,
//! a process-wide side effect per CLAUDE.md's isolated-test rules.
//!
//! # What is measured
//!
//! The workload `benches/throttle_check.rs` profiles: an inline
//! `#[throttle(limit = 1_000_000, per = "1h", key = "token")]` route at
//! `/route-a`, called with a bearer token, after its first request has built
//! and registered the route's limiter. Every measured call takes the real
//! `Decision::Allowed` path through a warm registry entry and a warm bucket.
//!
//! The limiter registry and the per-client bucket map are intern tables: once
//! a route's limiter and a client's bucket exist, a request must cost a
//! *lookup* in each, not a fresh owned copy of the key just to probe a map that
//! already holds an equal one. What is left per call is inherent to this
//! version of the check:
//!
//! - the client's bucket key, `token:<token>` (1), because the client key is
//!   derived per request;
//! - the `HeaderMap` copy `extract_throttle_key` makes to rebuild a request
//!   for `Limiter::extract_key` (2: its index table and its entry `Vec`).
//!
//! Allocations per warm allowed call. The totals are this gate run against the
//! code it replaced and against this one; the per-step split is read from the
//! code:
//!
//! | step                                            | before | after |
//! |-------------------------------------------------|--------|-------|
//! | registry namespace `route:{id}@{path}`          |      3 |     0 |
//! | registry cache key `{app}:{ns}@{fingerprint}`   |      3 |     0 |
//! | bearer token copy + `token:` key                |      3 |     1 |
//! | tenant fold-in copy of an unqualified key       |      1 |     0 |
//! | bucket-map `put` of an already-cached key       |      1 |     0 |
//! | `HeaderMap` copy in `extract_throttle_key`      |      2 |     2 |
//! | **total**                                       | **13** | **3** |
//!
//! A failure here means a refactor put an allocation back on the hit path;
//! re-measure and re-derive rather than raising the ceiling.

use autumn_web::AppState;
use autumn_web::security::{__check_throttle, KeyStrategy, ThrottleSpec};
use axum::http::{HeaderMap, HeaderValue, header::AUTHORIZATION};
use futures::FutureExt as _;

/// Enough calls that a per-call allocation cannot hide in noise.
const CALLS: u64 = 1_000;

/// The allocations a warm allowed call may still make; see the table above.
const MAX_PER_CALL: u64 = 3;

#[test]
#[allow(
    clippy::result_large_err,
    reason = "the closure only forwards `__check_throttle`, whose error is the 429 response"
)]
fn warm_throttle_check_allocates_only_the_client_key_and_header_copy() {
    let state = AppState::for_test();
    let mut headers = HeaderMap::new();
    // `from_static`, so the `HeaderMap` copy inside the check clones this
    // value as a pointer, not a promoted `Bytes`.
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer alloc-gate-client"),
    );
    let spec = ThrottleSpec::Inline {
        limit: 1_000_000,
        per_secs: 3_600,
        key: Some(KeyStrategy::ApiToken),
    };
    let check = || {
        __check_throttle(
            &state,
            "throttle_alloc_gate::warm",
            Some("/route-a"),
            spec.clone(),
            &headers,
            None,
            None,
            None,
            false,
        )
        .now_or_never()
        .expect("the memory backend never suspends")
    };

    // Warm up: the route's limiter, the client's bucket, the per-thread key
    // buffer, lazy statics and the first lock of each `Mutex`.
    for _ in 0..10 {
        assert!(check().is_ok(), "warm-up call must be allowed");
    }

    let info = allocation_counter::measure(|| {
        for _ in 0..CALLS {
            let result = check();
            std::hint::black_box(&result);
            assert!(result.is_ok(), "no measured call may be denied");
        }
    });
    let total = info.count_total;
    let per_call = total / CALLS;
    println!("warm #[throttle] check: {total} allocations over {CALLS} calls ({per_call}/call)");
    // Compared as a total: a quotient would round away a regression that
    // allocates on only some calls.
    assert!(
        total <= MAX_PER_CALL * CALLS,
        "a warm #[throttle] check made {total} allocations over {CALLS} calls, \
         ceiling {MAX_PER_CALL} per call"
    );
}
