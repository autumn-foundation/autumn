//! Baseline Chromium smoke — issue #1192.
//!
//! Spawns the real `blog` binary against an ephemeral testcontainer
//! Postgres (migrated automatically on boot — `AUTUMN_ENV=development`),
//! drives a headless-Chromium browser against the public index and the
//! pre-rendered `#[static_get]` about page, and asserts expected content
//! with no uncaught console errors.
//!
//! Run (requires Chromium + Docker):
//!   cargo test -p blog --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn blog_boots_and_serves_public_pages() {
    let db = example_e2e::provision_postgres(1).await;

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_blog"),
        env!("CARGO_MANIFEST_DIR"),
        &[("AUTUMN_DATABASE__URL", &db.urls()[0])],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn blog example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/").await.expect("visit /");
    page.expect_text("Welcome to the Blog")
        .await
        .expect("index heading renders");
    page.expect_text("No posts yet. Check back soon!")
        .await
        .expect("empty-state renders against the freshly migrated DB");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /");

    page.visit("/about").await.expect("visit /about");
    page.expect_text("About This Blog")
        .await
        .expect("pre-rendered about page renders");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /about");
}
