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
//! already holds an equal one. Nor may it copy the request to key it: the
//! headers, the peer address and the principal are read where they are. What is
//! left per call is inherent to the check: the client's bucket key,
//! `token:<token>` (1) or the client IP as text (up to 2), because the client
//! key is derived per request.
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
//! | `HeaderMap` copy in `extract_throttle_key`      |      2 |     0 |
//! | **total**                                       | **13** | **1** |
//!
//! A request with a socket peer made three more before, for the `ConnectInfo`
//! extension of the rebuilt request, and makes none of them now.
//!
//! A failure here means a refactor put an allocation back on the hit path;
//! re-measure and re-derive rather than raising the ceiling.

use autumn_web::AppState;
use autumn_web::security::{__check_throttle, KeyStrategy, ThrottleSpec};
use axum::http::{HeaderMap, HeaderName, HeaderValue, header::AUTHORIZATION};
use futures::FutureExt as _;

/// Enough calls that a per-call allocation cannot hide in noise.
const CALLS: u64 = 1_000;

/// The allocations a warm allowed call may still make; see the table above.
const MAX_PER_CALL: u64 = 1;

#[test]
#[allow(
    clippy::result_large_err,
    reason = "the closure only forwards `__check_throttle`, whose error is the 429 response"
)]
fn warm_throttle_check_allocates_only_the_client_key() {
    let state = AppState::for_test();
    let mut headers = HeaderMap::new();
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

/// The allocations a warm allowed call keyed by client IP may make: the IP
/// rendered as the bucket key. `IpAddr::to_string` writes the address piece by
/// piece, so an address longer than a `String`'s first eight bytes grows the
/// buffer once.
const MAX_PER_IP_KEYED_CALL: u64 = 2;

/// The shape a browser sends: a dozen header lines and a socket peer, keyed by
/// client IP. The check reads the headers and the peer in place, so its cost
/// does not grow with either: all that is allocated is the bucket key.
#[test]
#[allow(
    clippy::result_large_err,
    reason = "the closure only forwards `__check_throttle`, whose error is the 429 response"
)]
fn warm_throttle_check_does_not_copy_the_request_headers_or_peer() {
    let state = AppState::for_test();
    let mut headers = HeaderMap::new();
    for (name, value) in [
        ("host", "app.example"),
        (
            "user-agent",
            "Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101 Firefox/128.0",
        ),
        ("accept", "text/html,application/xhtml+xml"),
        ("accept-language", "en-GB,en;q=0.5"),
        ("accept-encoding", "gzip, deflate, br"),
        ("referer", "https://app.example/login"),
        ("cookie", "autumn.sid=0123456789abcdef; theme=dark"),
        ("x-forwarded-for", "203.0.113.9"),
        ("x-forwarded-proto", "https"),
        ("x-request-id", "0190d7c2-7f1e-7c3a-9d5e-2b1f4a6c8e90"),
        ("sec-fetch-site", "same-origin"),
        ("content-type", "application/x-www-form-urlencoded"),
    ] {
        // Owned names and values, as a parsed request carries them.
        headers.append(
            HeaderName::from_bytes(name.as_bytes()).expect("a valid header name"),
            HeaderValue::from_str(value).expect("a valid header value"),
        );
    }
    let peer = Some(std::net::SocketAddr::from(([198, 51, 100, 7], 54_321)));
    let spec = ThrottleSpec::Inline {
        limit: 1_000_000,
        per_secs: 3_600,
        key: Some(KeyStrategy::Ip),
    };
    let check = || {
        __check_throttle(
            &state,
            "throttle_alloc_gate::warm_browser",
            Some("/route-b"),
            spec.clone(),
            &headers,
            peer,
            None,
            None,
            false,
        )
        .now_or_never()
        .expect("the memory backend never suspends")
    };

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
    println!(
        "warm #[throttle] check, browser-shaped request: {total} allocations over {CALLS} calls"
    );
    assert!(
        total <= MAX_PER_IP_KEYED_CALL * CALLS,
        "a warm #[throttle] check of a browser-shaped request made {total} allocations over \
         {CALLS} calls, ceiling {MAX_PER_IP_KEYED_CALL} per call"
    );
}
