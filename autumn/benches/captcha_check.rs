//! Drives real requests through the production **bot-protection / CAPTCHA**
//! middleware (`autumn_web::security::BotProtectionLayer`,
//! `autumn::src::security::captcha::BotProtectionService`) so its per-request
//! cost can be profiled — the ingress-stack cost `request_pipeline.rs`
//! deliberately excludes (bot protection is opt-in via
//! `config.bot_protection.enabled` and no other committed bench touches it).
//!
//! `BotProtectionService::call` has four early-return branches before the
//! real token-scan/verification work (safe method, exempt path, dev bypass,
//! non-form content-type). This bench's `/route-a` exercises the **safe
//! method** branch — the one every plain `GET` takes on any site that turns
//! bot protection on, since Turnstile/hCaptcha widgets only ever guard
//! mutating `POST` forms. `/route-b` sends a `POST` with a valid token via
//! `TestCaptchaProvider`, so it runs the real verification path end to end,
//! as a no-regression control for the code the fix does not touch.
//!
//! Two otherwise-identical trivial handlers at equal-length paths with an
//! identical response body (`"ok"`) — the same discipline `throttle_check.rs`
//! documents — so the two routes differ only by the guard branch each one
//! hits, not by response size or path length.
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing beyond a sanity check that traffic isn't silently being denied: it
//! is a workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench captcha_check
//! BIN=$(find target/release/deps -maxdepth 1 -name "captcha_check-*" -type f ! -name "*.d")
//!
//! # Instruction profile — separate `--route` invocations so each isolates
//! # one branch's marginal cost (a combined `both` run interleaves one GET +
//! # one POST per round and cannot be split back apart afterward).
//! valgrind --tool=callgrind --callgrind-out-file=exempt-0.out    "$BIN" --iterations 0    --route exempt
//! valgrind --tool=callgrind --callgrind-out-file=exempt-1000.out "$BIN" --iterations 1000 --route exempt
//! callgrind_annotate --threshold=80 exempt-1000.out | head -40
//! # marginal Ir/request = (1000-iteration total - 0-iteration total) / 1000
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! # Two runs, subtracted, isolate the marginal per-request cost from process
//! # startup/router construction/warm-up (see `request_pipeline.rs`).
//! valgrind --tool=dhat --dhat-out-file=dhat-exempt-base.json "$BIN" --iterations 0   --route exempt
//! valgrind --tool=dhat --dhat-out-file=dhat-exempt-run.json  "$BIN" --iterations 200 --route exempt
//! # allocations/request = (total_run - total_base) / 200
//! ```
//!
//! `--iterations N` issues one request per round (route selected by
//! `--route`) after a fixed 50-round warm-up. `--route exempt|checked|both`
//! (default `both`) restricts the loop to one route, for an isolated
//! callgrind/dhat A/B.

use std::hint::black_box;

use autumn_web::prelude::*;
use autumn_web::security::{BotProtectionLayer, TestCaptchaProvider};
use autumn_web::test::TestApp;

// Equal-length paths (8 chars each) and an identical response body: the
// isolated A/B must differ only by which `BotProtectionService::call` branch
// each route takes, not by a longer route string or response payload padding
// out the byte count on one side (same discipline `throttle_check.rs`
// documents for its `/route-a` + `/route-b` pair).
#[get("/route-a")]
async fn exempt_get() -> &'static str {
    "ok"
}

#[post("/route-b")]
async fn checked_post() -> &'static str {
    "ok"
}

const VALID_TOKEN: &str = "bolt-captcha-bench-token";
const FORM_FIELD: &str = "cf-turnstile-response";

fn main() {
    // A single sequential pass over the full argument list — see
    // `throttle_check.rs` for why per-flag `.position()`/`.nth()` lookups
    // silently swallow a typo'd flag or a stray positional argument instead
    // of failing loudly.
    let mut iterations: u32 = 2_000;
    let mut route: String = "both".to_owned();
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw_args.len() {
        match raw_args[i].as_str() {
            "--iterations" => {
                let raw = raw_args.get(i + 1).expect("--iterations requires a value");
                iterations = raw.parse().unwrap_or_else(|e| {
                    panic!("--iterations value {raw:?} is not a valid u32: {e}")
                });
                i += 2;
            }
            "--route" => {
                raw_args
                    .get(i + 1)
                    .expect("--route requires a value")
                    .clone_into(&mut route);
                i += 2;
            }
            other => panic!(
                "unrecognized argument {other:?}; this bench only accepts \
                 --iterations <N> and --route exempt|checked|both"
            ),
        }
    }

    let (hit_exempt, hit_checked) = match route.as_str() {
        "exempt" => (true, false),
        "checked" => (false, true),
        "both" => (true, true),
        other => panic!("--route must be one of exempt|checked|both, got {other:?}"),
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let layer = BotProtectionLayer::new(std::sync::Arc::new(TestCaptchaProvider::new(VALID_TOKEN)));
    let client = TestApp::new()
        .layer(layer)
        .routes(routes![exempt_get, checked_post])
        .build();

    let checked_body = format!("{FORM_FIELD}={VALID_TOKEN}");

    rt.block_on(async {
        for _ in 0..50 {
            if hit_exempt {
                let resp = client.get("/route-a").send().await;
                assert_eq!(
                    resp.status,
                    StatusCode::OK,
                    "warm-up GET must not be denied"
                );
            }
            if hit_checked {
                let resp = client
                    .post("/route-b")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(checked_body.clone())
                    .send()
                    .await;
                assert_eq!(
                    resp.status,
                    StatusCode::OK,
                    "warm-up POST must pass verification"
                );
            }
        }

        for _ in 0..iterations {
            if hit_exempt {
                let resp = client.get("/route-a").send().await;
                // Asserted, not just `black_box`ed: a silent denial partway
                // through a long run would corrupt every DHAT/callgrind
                // number after it without this failing loudly (same
                // reasoning `throttle_check.rs` documents).
                assert_eq!(
                    resp.status,
                    StatusCode::OK,
                    "measured GET was unexpectedly denied"
                );
                black_box(resp.status);
            }
            if hit_checked {
                let resp = client
                    .post("/route-b")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(checked_body.clone())
                    .send()
                    .await;
                assert_eq!(
                    resp.status,
                    StatusCode::OK,
                    "measured POST was unexpectedly denied"
                );
                black_box(resp.status);
            }
        }
    });

    let per_round = u32::from(hit_exempt) + u32::from(hit_checked);
    println!(
        "completed {} requests",
        iterations * per_round + 50 * per_round
    );
}
