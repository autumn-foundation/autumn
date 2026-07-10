//! Baseline Chromium smoke — mirrors the other supported examples.
//!
//! Spawns the real `flock` binary (built normally by cargo, routes
//! untouched) on an ephemeral port, drives a headless-Chromium browser
//! against its single `GET /` page, and asserts that the WASM island
//! actually boots — not just that the server-rendered mount marker is
//! present.
//!
//! The island is a client-side Yew CSR "literary boids" animation. The
//! server sends only an empty `<div data-autumn-island="flock">` mount
//! point; everything inside it (the control strip, the live readout, the
//! `<canvas id="flock-canvas">`) is produced by the WASM bundle *after* it
//! loads and mounts. Asserting on that post-mount DOM — rather than on the
//! static marker emitted before any client code runs — is what makes this a
//! real island smoke: it fails if the wasm bundle is missing, broken, or
//! blocked by CSP. The `expect_text`/`expect_attribute` assertions poll (5 s
//! deadline) so the async wasm load isn't racy, and `expect_no_console_errors`
//! catches a bundle that failed to fetch or `init()` at all.
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

    // Everything below only exists after the Yew island has loaded and
    // rendered into the mount element client-side — a missing, broken, or
    // CSP-blocked wasm bundle leaves the mount point empty and fails here.
    //
    // The live readout string is produced by the component's `html!` (see
    // examples/island-flock/src/lib.rs); its distinctive "corpus: Autumn
    // source" tail is server-impossible, so seeing it proves the wasm ran.
    page.expect_text("corpus: Autumn source")
        .await
        .expect("island readout renders — wasm mounted");
    // The island inserts its own `<canvas id="flock-canvas">` inside the
    // mount element; asserting the canvas exists *inside* the marker div
    // confirms the component body (not just the static wrapper) rendered.
    page.expect_attribute("[data-autumn-island=\"flock\"] canvas", "id", "flock-canvas")
        .await
        .expect("island canvas mounted inside the marker element");
    page.expect_no_console_errors()
        .await
        .expect("no console errors — wasm bundle fetched, init()'d, and mounted cleanly");
}
