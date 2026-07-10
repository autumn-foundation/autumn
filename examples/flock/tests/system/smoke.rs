//! Baseline Chromium smoke — mirrors the other supported examples.
//!
//! Spawns the real `flock` binary (built normally by cargo, routes
//! untouched) on an ephemeral port, drives a headless-Chromium browser
//! against its single `GET /` page, and asserts the server-rendered page
//! boots and carries the WASM-island mount marker.
//!
//! The island itself is a client-side Yew CSR "literary boids" animation
//! whose runtime behaviour is out of scope for a boot smoke, so this
//! asserts on the deterministic, server-rendered markup — the page
//! heading and the `data-autumn-island="flock"` mount point — rather than
//! on the WASM having executed.
//!
//! Run (requires Chromium):
//!   cargo test -p flock --features system-tests --test smoke -- --include-ignored
//!
//!   AUTUMN_CHROMIUM=/path/to/chrome cargo test ...   # custom binary
#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn flock_boots_and_serves_index() {
    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_flock"),
        env!("CARGO_MANIFEST_DIR"),
        &[],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn flock example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/").await.expect("visit /");
    page.expect_text("Literary boids")
        .await
        .expect("index route renders");
    page.expect_attribute("[data-autumn-island]", "data-autumn-island", "flock")
        .await
        .expect("index page carries the WASM-island mount marker");
}
