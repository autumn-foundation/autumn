//! Baseline Chromium smoke — issue #1192.
//!
//! Spawns the real `bookmarks` binary against an ephemeral testcontainer
//! Postgres (migrated automatically on boot — `AUTUMN_ENV=development`),
//! drives a headless-Chromium browser against the bookmarks list and the
//! actuator health endpoint, and asserts expected content with no uncaught
//! console errors.
//!
//! It also closes the loop on the app-metrics facade (#1378): visiting
//! `/bookmarks/stats` runs the timer call site in `routes::bookmarks::stats`
//! against a real database, and `/actuator/prometheus` must then expose that
//! instrument. `src/metrics.rs`'s own tests assert the same families against a
//! stock `TestApp`; this is the end-to-end proof that the real binary, booted
//! with the real profile, exposes them too.
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

    // Run the timer call site: /bookmarks/stats issues the two grouped
    // aggregates the `bookmark_stats_query_seconds` histogram measures.
    page.visit("/bookmarks/stats")
        .await
        .expect("visit /bookmarks/stats");
    page.expect_text("Bookmark stats")
        .await
        .expect("stats roll-up renders");

    // The app's own instruments land on the same scrape endpoint as the
    // framework's built-in `autumn_http_*` families.
    page.visit("/actuator/prometheus")
        .await
        .expect("visit /actuator/prometheus");
    // Only the timer is asserted here. `describe()` attaches HELP text but does
    // not register an instrument, so `bookmarks_created_total` stays out of the
    // scrape entirely until a submission actually records into it — and this
    // smoke never posts the create form. `src/metrics.rs`'s tests cover the
    // counter.
    page.expect_text("bookmark_stats_query_seconds_count")
        .await
        .expect("the app timer recorded by /bookmarks/stats is scrapeable");

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
