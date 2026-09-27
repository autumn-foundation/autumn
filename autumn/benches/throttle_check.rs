//! Drives real requests through the production `#[throttle]` per-route rate
//! limiter (`autumn_web::security::rate_limit::__check_throttle`, issue
//! #1350) so its per-request cost can be profiled — the ingress-stack cost
//! `request_pipeline.rs` deliberately excludes (its three handlers carry no
//! `#[throttle]` attribute) and that no other committed bench touches.
//!
//! Two otherwise-identical trivial handlers are mounted at equal-length paths
//! with an identical response body (`"ok"`), so the only difference between
//! them is the guard under test: `/route-a` carries
//! `#[throttle(limit = 1_000_000, per = "1h", key = "token")]` (a limit high
//! enough that no request in this bench is ever denied — every call takes the
//! real `Decision::Allowed` steady-state path through a WARM registry entry,
//! not the one-time `Limiter` construction on the first request to the
//! route), `/route-b` has no throttle at all. `key = "token"` rather than the
//! more obvious `key = "ip"`: `TestApp`'s in-process requests carry no
//! `ConnectInfo` and this bench configures no trusted-proxy forwarding
//! headers, so an IP-keyed limiter's `extract_throttle_key` would return
//! `None` on every call and `__check_throttle` would take its no-client
//! bypass — profiling that early-return instead of the real bucket
//! lookup/lock/refill path `limiter.decide()` does (caught in review on the
//! first version of this bench). Every request to EITHER route carries the
//! identical `Authorization: Bearer <token>` header — `/route-b` ignores it,
//! but sending it there too means the two workloads differ only by the
//! `#[throttle]` guard itself, not by one of them also paying for building
//! and parsing an extra header, or for a longer route path / response body
//! (both also caught in review: an earlier version sent the header to
//! `/throttled` only, and used differently-sized `/throttled`+`"throttled-ok"`
//! vs. `/plain`+`"plain-ok"` route/body pairs, folding those differences into
//! the "cost of `#[throttle]`" measurement). `extract_bearer_token` reads the
//! header directly off the request with no dependency on peer/proxy info, so
//! `limiter.decide()` genuinely runs on every measured `/route-a` call. Both
//! routes are driven through the real production router via
//! `TestApp::build()` (same `try_build_router_inner` production apps use),
//! interleaved in the same run, so DHAT's per-route marginal allocation
//! difference isolates exactly what `#[throttle]` adds on top of the same
//! ingress stack `request_pipeline.rs` already profiles — an isolated A/B, no
//! framework code changed for this bench.
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing beyond a sanity check that traffic isn't silently being denied: it
//! is a workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench throttle_check
//! BIN=$(find target/release/deps -maxdepth 1 -name "throttle_check-*" -type f ! -name "*.d")
//!
//! # Instruction profile — separate `--route` invocations, not the `both`
//! # default, so the two output files are the isolated per-route workloads
//! # the "own added cost" comparison below actually needs (a combined `both`
//! # run profiles one throttled + one plain request per iteration together
//! # and cannot be split back apart after the fact — caught in review).
//! # Callgrind collects from process startup by default, so each nonzero-
//! # iteration total still includes router construction and the 50-round
//! # warm-up; a zero-iteration run per route isolates that fixed cost so it
//! # can be subtracted before dividing by the iteration count, the same way
//! # the dhat runs below already are (also caught in review — an earlier
//! # version of this doc example divided the raw nonzero totals directly).
//! valgrind --tool=callgrind --callgrind-out-file=throttled-0.out    "$BIN" --iterations 0    --route throttled
//! valgrind --tool=callgrind --callgrind-out-file=plain-0.out        "$BIN" --iterations 0    --route plain
//! valgrind --tool=callgrind --callgrind-out-file=throttled-1000.out "$BIN" --iterations 1000 --route throttled
//! valgrind --tool=callgrind --callgrind-out-file=plain-1000.out     "$BIN" --iterations 1000 --route plain
//! callgrind_annotate --threshold=80 throttled-1000.out | head -40
//! callgrind_annotate --threshold=80 plain-1000.out     | head -40
//! # marginal Ir/request = (1000-iteration total - 0-iteration total) / 1000
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! # Two runs per route, subtracted, isolate the marginal per-request cost
//! # from process-startup/router-construction/warm-up (see
//! # `request_pipeline.rs`), then diff the two routes' marginals for
//! # `#[throttle]`'s own added cost.
//! valgrind --tool=dhat --dhat-out-file=dhat-throttled-base.json "$BIN" --iterations 0   --route throttled
//! valgrind --tool=dhat --dhat-out-file=dhat-throttled-run.json  "$BIN" --iterations 200 --route throttled
//! valgrind --tool=dhat --dhat-out-file=dhat-plain-base.json     "$BIN" --iterations 0   --route plain
//! valgrind --tool=dhat --dhat-out-file=dhat-plain-run.json      "$BIN" --iterations 200 --route plain
//! ```
//!
//! `--iterations N` issues one `/route-a` + one `/route-b` GET per round
//! after a fixed 50-round warm-up (which also warms the throttle registry
//! entry so the measured rounds never pay the one-time `Limiter::new` cost).
//! Rejects an `N` large enough to drain the `#[throttle]` bucket (see
//! `THROTTLE_LIMIT`) rather than silently profiling denials.
//! `--route throttled|plain|both` (default `both`) restricts the loop to one
//! route, for an isolated DHAT A/B: run `--route throttled` and `--route
//! plain` separately and diff their marginal (`--iterations N` minus
//! `--iterations 0`) block/byte counts to get `#[throttle]`'s own added cost.

use std::hint::black_box;

use autumn_web::prelude::*;
use autumn_web::test::TestApp;

// Equal-length paths (8 chars each) and an identical response body: the
// isolated DHAT A/B must differ only by the `#[throttle]` guard, not by a
// longer route string or response payload padding out the byte count on one
// side (caught in review — an earlier version used `/throttled` +
// `"throttled-ok"` vs. `/plain` + `"plain-ok"`, folding those length
// differences into the "cost of `#[throttle]`" measurement).
#[get("/route-a")]
#[throttle(limit = 1_000_000, per = "1h", key = "token")]
async fn throttled() -> &'static str {
    "ok"
}

#[get("/route-b")]
async fn plain() -> &'static str {
    "ok"
}

const BEARER_TOKEN: &str = "bolt-throttle-bench-client";

/// Keeps the measured loop's total `/throttled` traffic (plus the fixed
/// 50-round warm-up) safely under the configured `limit`, so a large
/// `--iterations` value can never drain the token bucket mid-run and start
/// silently profiling `Decision::Denied` responses instead of the documented
/// warm `Decision::Allowed` path (caught in review).
const THROTTLE_LIMIT: u32 = 1_000_000;

fn main() {
    // A single sequential pass over the full argument list, rather than
    // separate `.position()`/`.nth()` lookups per flag: those only matched
    // the exact flag spelling, so a typo'd flag name (`--rouet`, `--iteratoin`)
    // or a stray positional argument was invisible to both lookups and left
    // every flag silently at its default — the same "wrong workload, no
    // error" risk already fixed per-flag for a malformed *value*, just one
    // level up, at the flag name itself (caught in review). Every argument
    // must now be a recognized flag or a value immediately following one;
    // anything else panics naming it.
    let mut iterations: u32 = 2_000;
    let mut route: String = "both".to_owned();
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw_args.len() {
        match raw_args[i].as_str() {
            "--iterations" => {
                let raw = raw_args.get(i + 1).expect("--iterations requires a value");
                // `.parse().ok()` alone would collapse a malformed value (a
                // typo like `20O0`) into `None`, silently keeping the
                // 2,000-iteration default for a workload the caller didn't
                // ask for (caught in review).
                iterations = raw.parse().unwrap_or_else(|e| {
                    panic!("--iterations value {raw:?} is not a valid u32: {e}")
                });
                i += 2;
            }
            "--route" => {
                // `--route throttled|plain|both` (default `both`) isolates
                // one route's marginal DHAT cost from the other's — an A/B
                // knob for the profiler, like `repository_crud.rs`'s
                // `--fast-recycle`. No framework code changes with this
                // flag; it only decides which of the two already-mounted
                // routes this run's loop sends traffic to.
                raw_args
                    .get(i + 1)
                    .expect("--route requires a value")
                    .clone_into(&mut route);
                i += 2;
            }
            other => panic!(
                "unrecognized argument {other:?}; this bench only accepts \
                 --iterations <N> and --route throttled|plain|both"
            ),
        }
    }
    // Compared as `iterations < THROTTLE_LIMIT - 50` (a compile-time-constant
    // subtraction), not `iterations + 50 < THROTTLE_LIMIT`: the addition form
    // wraps silently in a release build for an `iterations` near `u32::MAX`
    // (e.g. `--iterations 4294967295` wraps to 49, passing the guard and
    // starting an enormous run that reaches the denial path anyway) — caught
    // in review.
    assert!(
        iterations < THROTTLE_LIMIT - 50,
        "--iterations {iterations} plus the 50-round warm-up would exhaust the \
         #[throttle] bucket (limit = {THROTTLE_LIMIT}), profiling denials instead \
         of the intended warm Decision::Allowed path"
    );

    // An unrecognized `--route` value is rejected rather than silently
    // falling back to `both` (caught in review): a typo'd value would
    // otherwise record a mixed, doubled workload and produce an invalid A/B
    // with no indication why.
    let (hit_throttled, hit_plain) = match route.as_str() {
        "throttled" => (true, false),
        "plain" => (false, true),
        "both" => (true, true),
        other => panic!("--route must be one of throttled|plain|both, got {other:?}"),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let client = TestApp::new().routes(routes![throttled, plain]).build();

    rt.block_on(async {
        for _ in 0..50 {
            if hit_throttled {
                let resp = client
                    .get("/route-a")
                    .header("authorization", &format!("Bearer {BEARER_TOKEN}"))
                    .send()
                    .await;
                assert_eq!(resp.status, StatusCode::OK, "warm-up must not be denied");
            }
            if hit_plain {
                let resp = client
                    .get("/route-b")
                    .header("authorization", &format!("Bearer {BEARER_TOKEN}"))
                    .send()
                    .await;
                assert_eq!(resp.status, StatusCode::OK, "warm-up baseline must succeed");
            }
        }

        for _ in 0..iterations {
            if hit_throttled {
                let resp = client
                    .get("/route-a")
                    .header("authorization", &format!("Bearer {BEARER_TOKEN}"))
                    .send()
                    .await;
                // Asserted, not just `black_box`ed: a silent `Decision::Denied`
                // partway through a long run would corrupt every DHAT/callgrind
                // number after it without this failing loudly (caught in
                // review). The `THROTTLE_LIMIT` guard above should make this
                // unreachable; this is the backstop. The `/route-b` arm below
                // asserts too, not just `black_box`es — an asymmetric check
                // would put the assertion's own comparison/branch instructions
                // on only one side of the `--route throttled` vs. `--route
                // plain` callgrind delta, again not `#[throttle]` itself
                // (caught in review).
                assert_eq!(
                    resp.status,
                    StatusCode::OK,
                    "measured request was denied — the throttle bucket ran dry"
                );
                black_box(resp.status);
            }
            if hit_plain {
                let resp = client
                    .get("/route-b")
                    .header("authorization", &format!("Bearer {BEARER_TOKEN}"))
                    .send()
                    .await;
                assert_eq!(
                    resp.status,
                    StatusCode::OK,
                    "measured baseline request unexpectedly failed"
                );
                black_box(resp.status);
            }
        }
    });

    let per_round = u32::from(hit_throttled) + u32::from(hit_plain);
    println!(
        "completed {} requests",
        iterations * per_round + 50 * per_round
    );
}
