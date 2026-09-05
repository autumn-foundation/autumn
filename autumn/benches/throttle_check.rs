//! Drives real requests through the production `#[throttle]` per-route rate
//! limiter (`autumn_web::security::rate_limit::__check_throttle`, issue
//! #1350) so its per-request cost can be profiled — the ingress-stack cost
//! `request_pipeline.rs` deliberately excludes (its three handlers carry no
//! `#[throttle]` attribute) and that no other committed bench touches.
//!
//! Two otherwise-identical trivial handlers are mounted: `/throttled` carries
//! `#[throttle(limit = 1_000_000, per = "1h", key = "token")]` (a limit high
//! enough that no request in this bench is ever denied — every call takes the
//! real `Decision::Allowed` steady-state path through a WARM registry entry,
//! not the one-time `Limiter` construction on the first request to the
//! route), `/plain` has no throttle at all. `key = "token"` rather than the
//! more obvious `key = "ip"`: `TestApp`'s in-process requests carry no
//! `ConnectInfo` and this bench configures no trusted-proxy forwarding
//! headers, so an IP-keyed limiter's `extract_throttle_key` would return
//! `None` on every call and `__check_throttle` would take its no-client
//! bypass — profiling that early-return instead of the real bucket
//! lookup/lock/refill path `limiter.decide()` does (caught in review on the
//! first version of this bench). Every request to EITHER route carries the
//! identical `Authorization: Bearer <token>` header — `/plain` ignores it,
//! but sending it there too means the two workloads differ only by the
//! `#[throttle]` guard itself, not by one of them also paying for building
//! and parsing an extra header (also caught in review: an earlier version
//! sent the header to `/throttled` only, which folded the header's own
//! allocation and ingress cost into the "cost of `#[throttle]`" measurement).
//! `extract_bearer_token` reads the header directly off the request with no
//! dependency on peer/proxy info, so `limiter.decide()` genuinely runs on
//! every measured `/throttled` call. Both routes are driven through the real
//! production router via `TestApp::build()` (same `try_build_router_inner`
//! production apps use), interleaved in the same run, so DHAT's per-route
//! marginal allocation difference isolates exactly what `#[throttle]` adds on
//! top of the same ingress stack `request_pipeline.rs` already profiles — an
//! isolated A/B, no framework code changed for this bench.
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing beyond a sanity check that traffic isn't silently being denied: it
//! is a workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench throttle_check
//! BIN=$(find target/release/deps -maxdepth 1 -name "throttle_check-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --iterations 1000
//! callgrind_annotate --threshold=80 callgrind.out | head -40
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! # Two runs, subtracted, isolate the marginal per-request cost from
//! # process-startup/router-construction/warm-up (see `request_pipeline.rs`).
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 200
//! ```
//!
//! `--iterations N` issues one `/throttled` + one `/plain` GET per round
//! after a fixed 50-round warm-up (which also warms the throttle registry
//! entry so the measured rounds never pay the one-time `Limiter::new` cost).
//! `--route throttled|plain|both` (default `both`) restricts the loop to one
//! route, for an isolated DHAT A/B: run `--route throttled` and `--route
//! plain` separately and diff their marginal (`--iterations N` minus
//! `--iterations 0`) block/byte counts to get `#[throttle]`'s own added cost.

use std::hint::black_box;

use autumn_web::prelude::*;
use autumn_web::test::TestApp;

#[get("/throttled")]
#[throttle(limit = 1_000_000, per = "1h", key = "token")]
async fn throttled() -> &'static str {
    "throttled-ok"
}

#[get("/plain")]
async fn plain() -> &'static str {
    "plain-ok"
}

const BEARER_TOKEN: &str = "bolt-throttle-bench-client";

fn main() {
    let iterations: u32 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);

    // `--route throttled|plain|both` (default `both`) isolates one route's
    // marginal DHAT cost from the other's — an A/B knob for the profiler,
    // like `repository_crud.rs`'s `--fast-recycle`. No framework code changes
    // with this flag; it only decides which of the two already-mounted
    // routes this run's loop sends traffic to.
    let route: String = std::env::args()
        .position(|a| a == "--route")
        .and_then(|i| std::env::args().nth(i + 1))
        .unwrap_or_else(|| "both".to_owned());
    let (hit_throttled, hit_plain) = match route.as_str() {
        "throttled" => (true, false),
        "plain" => (false, true),
        _ => (true, true),
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
                    .get("/throttled")
                    .header("authorization", &format!("Bearer {BEARER_TOKEN}"))
                    .send()
                    .await;
                assert_eq!(resp.status, StatusCode::OK, "warm-up must not be denied");
            }
            if hit_plain {
                let resp = client
                    .get("/plain")
                    .header("authorization", &format!("Bearer {BEARER_TOKEN}"))
                    .send()
                    .await;
                assert_eq!(resp.status, StatusCode::OK, "warm-up baseline must succeed");
            }
        }

        for _ in 0..iterations {
            if hit_throttled {
                black_box(
                    client
                        .get("/throttled")
                        .header("authorization", &format!("Bearer {BEARER_TOKEN}"))
                        .send()
                        .await
                        .status,
                );
            }
            if hit_plain {
                black_box(
                    client
                        .get("/plain")
                        .header("authorization", &format!("Bearer {BEARER_TOKEN}"))
                        .send()
                        .await
                        .status,
                );
            }
        }
    });

    let per_round = u32::from(hit_throttled) + u32::from(hit_plain);
    println!(
        "completed {} requests",
        iterations * per_round + 50 * per_round
    );
}
