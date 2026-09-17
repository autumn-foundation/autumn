//! Drives real GET traffic through the production **bot-protection / CAPTCHA**
//! middleware (`autumn_web::security::captcha::BotProtectionService`) so its
//! per-request cost can be profiled — the ingress-stack cost
//! `request_pipeline.rs` deliberately excludes (bot protection is opt-in via
//! `config.bot_protection.enabled` and no other committed bench touches it).
//!
//! `BotProtectionService::call` has four early-return branches before the
//! real token-scan/verification work (safe method, exempt path, dev bypass,
//! non-form content-type). This bench exercises the **safe method** branch —
//! the one every plain `GET` takes on any site that turns bot protection on,
//! since Turnstile/hCaptcha widgets only ever guard mutating `POST` forms.
//!
//! Mounted via `TestApp::config(...)` with `bot_protection.enabled = true`
//! (`dev_bypass = true` so no outbound network call to a real Turnstile/
//! hCaptcha endpoint is needed — irrelevant to what's measured here anyway,
//! since the safe-method branch returns before `dev_bypass` is even checked),
//! the same convention `csrf_verify.rs` uses for `security.csrf.enabled`.
//! This is load-bearing, not stylistic: going through `AutumnConfig` routes
//! construction through the real `build_bot_protection_layer` +
//! `try_build_router_inner` path (issue #2193's tuple-composed `inner_stack`,
//! ONE `.layer()` call for the whole group), so `BotProtectionService::inner`
//! is the concrete next-layer type in that tuple. An earlier version of this
//! bench instead attached `BotProtectionLayer` via `TestApp::layer(...)`
//! (`AppBuilder::layer`'s `IntoAppLayer` slot), which type-erases the layer's
//! wrapped service to `ErasedAppService` (`BoxCloneSyncService`) — making
//! `self.inner.clone()` hit `CloneService::clone_box`'s box allocation
//! regardless of what the fix under test does, an artifact of *that* slot's
//! erasure boundary rather than of the production mount point. Caught in
//! review (Codex) on the first version of this bench.
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing beyond a sanity check that traffic isn't silently being denied: it
//! is a workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench captcha_check
//! BIN=$(find target/release/deps -maxdepth 1 -name "captcha_check-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind-0.out    "$BIN" --iterations 0
//! valgrind --tool=callgrind --callgrind-out-file=callgrind-1000.out "$BIN" --iterations 1000
//! callgrind_annotate --threshold=80 callgrind-1000.out | head -40
//! # marginal Ir/request = (1000-iteration total - 0-iteration total) / 1000
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! # Two runs, subtracted, isolate the marginal per-request cost from process
//! # startup/router construction/warm-up (see `request_pipeline.rs`).
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 200
//! # allocations/request = (total_run - total_base) / 200
//! ```
//!
//! `--iterations N` issues one GET per round after a fixed 50-round warm-up.

use std::hint::black_box;

use autumn_web::config::AutumnConfig;
use autumn_web::prelude::*;
use autumn_web::security::BotProtectionConfig;
use autumn_web::test::TestApp;

#[get("/notes")]
async fn list_notes() -> &'static str {
    "ok"
}

fn main() {
    let iterations: u32 = std::env::args()
        .position(|a| a == "--iterations")
        .and_then(|i| std::env::args().nth(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    let config = AutumnConfig {
        profile: Some("test".into()),
        bot_protection: BotProtectionConfig {
            enabled: true,
            dev_bypass: true,
            ..BotProtectionConfig::default()
        },
        ..AutumnConfig::default()
    };

    let client = TestApp::new()
        .config(config)
        .routes(routes![list_notes])
        .build();

    rt.block_on(async {
        for _ in 0..50 {
            let resp = client.get("/notes").send().await;
            assert_eq!(
                resp.status,
                StatusCode::OK,
                "warm-up GET must not be denied"
            );
        }

        for _ in 0..iterations {
            let resp = client.get("/notes").send().await;
            // Asserted, not just `black_box`ed: a silent denial partway
            // through a long run would corrupt every DHAT/callgrind number
            // after it without this failing loudly (same reasoning
            // `throttle_check.rs` documents).
            assert_eq!(
                resp.status,
                StatusCode::OK,
                "measured GET was unexpectedly denied"
            );
            black_box(resp.status);
        }
    });

    println!("completed {} requests", iterations + 50);
}
