//! Baseline Chromium smoke — issue #1192.
//!
//! Spawns the real `bookmarks` binary against an ephemeral testcontainer
//! Postgres (migrated automatically on boot — `AUTUMN_ENV=development`),
//! drives a headless-Chromium browser against the bookmarks list and the
//! actuator health endpoint, and asserts expected content with no uncaught
//! console errors.
//!
//! Run (requires Chromium + Docker):
//!   cargo test -p bookmarks --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn bookmarks_boots_and_serves_list_and_health() {
    let db = example_e2e::provision_postgres(1).await;

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_bookmarks"),
        env!("CARGO_MANIFEST_DIR"),
        &[("AUTUMN_DATABASE__URL", &db.urls()[0])],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn bookmarks example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/bookmarks").await.expect("visit /bookmarks");
    page.expect_text("All Bookmarks")
        .await
        .expect("bookmarks list heading renders");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /bookmarks");

    page.visit("/actuator/health")
        .await
        .expect("visit /actuator/health");
    page.expect_text("UP")
        .await
        .expect("actuator health reports UP against the freshly migrated DB");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /actuator/health");
}
