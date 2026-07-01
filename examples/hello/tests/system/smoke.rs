//! Baseline Chromium smoke — issue #1192.
//!
//! Spawns the real `hello` binary (built normally by cargo, routes
//! untouched) on an ephemeral port, drives a headless-Chromium browser
//! against its primary routes, and asserts expected content with no
//! uncaught console errors.
//!
//! Run (requires Chromium):
//!   cargo test -p hello --features system-tests --test smoke -- --include-ignored
//!
//!   AUTUMN_CHROMIUM=/path/to/chrome cargo test ...   # custom binary

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn hello_boots_and_serves_primary_routes() {
    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_hello"),
        env!("CARGO_MANIFEST_DIR"),
        &[],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn hello example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/").await.expect("visit /");
    page.expect_text("Welcome to Autumn!")
        .await
        .expect("index route renders");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /");

    page.visit("/hello").await.expect("visit /hello");
    page.expect_text("Hello, Autumn!")
        .await
        .expect("/hello route renders");

    page.visit("/hello/World")
        .await
        .expect("visit /hello/World");
    page.expect_text("Hello, World!")
        .await
        .expect("/hello/{name} route renders with the path param");
    page.expect_no_console_errors()
        .await
        .expect("no console errors after navigating between routes");
}
