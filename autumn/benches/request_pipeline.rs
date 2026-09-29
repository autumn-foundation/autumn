//! Drives real requests through the **production** router so the framework's
//! per-request ingress cost can be profiled (issue #2193).
//!
//! `TestApp::build()` calls the same `try_build_router_inner` a deployed app
//! uses, so this exercises the documented ingress stack
//! (`SecurityHeaders → … → Metrics → ExceptionFilter → ErrorPageContext →
//! Session → RequestId → LogContext → AccessLog → … → CORS → handler`) rather
//! than a stripped-down mock. The three handlers are deliberately trivial —
//! plain-text GET, GET with a `Path` extractor, POST with a JSON body — so what
//! is measured is the tax every request pays *around* the handler, not
//! application logic.
//!
//! What it does NOT cover: the layers `App::run` wraps around the finished
//! router at the `axum::serve` boundary (`MethodOverrideLayer`,
//! `TrustedProxiesLayer`) and `apply_startup_barrier`'s outermost quartet, none
//! of which `try_build_router_inner` installs. A deployed request pays a little
//! more than what is measured here.
//!
//! Like the other benches in this crate it is `harness = false` and asserts
//! nothing: it is a workload to point a profiler at.
//!
//! ```sh
//! cargo build --release -p autumn-web --bench request_pipeline
//! BIN=$(find target/release/deps -maxdepth 1 -name "request_pipeline-*" -type f ! -name "*.d")
//!
//! # Instruction profile
//! valgrind --tool=callgrind --callgrind-out-file=callgrind.out "$BIN" --iterations 1000
//! callgrind_annotate --threshold=80 callgrind.out | head -40
//!
//! # Allocation profile (valgrind's built-in dhat tool — no crate dependency).
//! # Take TWO runs and subtract: `--iterations 0` measures process startup plus
//! # router construction plus the warm-up, so subtracting it leaves the MARGINAL
//! # per-request cost rather than one amortised over the run length.
//! valgrind --tool=dhat --dhat-out-file=dhat-base.json "$BIN" --iterations 0
//! valgrind --tool=dhat --dhat-out-file=dhat-run.json  "$BIN" --iterations 200
//! # allocations/request = (total_run - total_base) / (3 * 200)
//! # ("total" is the block count DHAT prints as "Total: N bytes in M blocks".)
//! ```
//!
//! `--iterations N` issues `3 * N` requests after a fixed 50-request warm-up.

use std::hint::black_box;

use autumn_web::prelude::*;
use autumn_web::test::TestApp;

#[get("/")]
async fn index() -> &'static str {
    "Welcome to Autumn!"
}

#[get("/users/{id}")]
async fn show_user(Path(id): Path<u32>) -> String {
    format!("user #{id}")
}

#[post("/users")]
async fn create_user(Json(payload): Json<serde_json::Value>) -> (StatusCode, String) {
    let name = payload["name"].as_str().unwrap_or("anon").to_owned();
    (StatusCode::CREATED, format!("created {name}"))
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

    let client = TestApp::new()
        .routes(routes![index, show_user, create_user])
        .build();

    rt.block_on(async {
        for _ in 0..50 {
            black_box(client.get("/").send().await.status);
        }
        for i in 0..iterations {
            black_box(client.get("/").send().await.status);
            black_box(client.get(&format!("/users/{i}")).send().await.status);
            black_box(
                client
                    .post("/users")
                    .json(&serde_json::json!({ "name": format!("user-{i}") }))
                    .send()
                    .await
                    .status,
            );
        }
    });

    println!("completed {} requests", iterations * 3 + 50);
}
