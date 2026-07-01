//! Baseline Chromium smoke — issue #1192.
//!
//! Spawns the real `reddit-clone` binary — autumn's canonical
//! feature-showcase example (jobs, flags, experiments, sessions, CSRF,
//! htmx, ...) — against an ephemeral testcontainer Postgres (migrated
//! automatically on boot — `AUTUMN_ENV=development`), drives a
//! headless-Chromium browser against the front page and the public login
//! page, and asserts expected content with no uncaught console errors.
//!
//! This is the highest-value smoke in the fleet: reddit-clone's `main.rs`
//! wires up more of the framework than any other example (flag store,
//! experiment service, error reporter, Postgres-backed job runtime), so
//! booting it for real is the strongest single "did anything rot" signal.
//!
//! Run (requires Chromium + Docker):
//!   cargo test -p reddit-clone --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn reddit_clone_boots_and_serves_front_page() {
    let db = example_e2e::provision_postgres(1).await;

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_reddit-clone"),
        env!("CARGO_MANIFEST_DIR"),
        &[("AUTUMN_DATABASE__URL", &db.urls()[0])],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn reddit-clone example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/").await.expect("visit /");
    page.expect_text("autumn/reddit")
        .await
        .expect("nav brand renders — real routes + state booted");
    page.expect_text("Hot")
        .await
        .expect("front page sort tabs render");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on the front page");

    page.visit("/login").await.expect("visit /login");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /login");
}
