//! Baseline Chromium smoke — issue #1192.
//!
//! Spawns the real `wiki` binary against an ephemeral testcontainer
//! Postgres (migrated automatically on boot — `AUTUMN_ENV=development`),
//! drives a headless-Chromium browser against the page list, and asserts
//! expected content with no uncaught console errors.
//!
//! Run (requires Chromium + Docker):
//!   cargo test -p wiki --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn wiki_boots_and_serves_page_list() {
    let db = example_e2e::provision_postgres(1).await;

    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_wiki"),
        env!("CARGO_MANIFEST_DIR"),
        &[("AUTUMN_DATABASE__URL", &db.urls()[0])],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn wiki example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/").await.expect("visit /");
    page.expect_text("All Pages")
        .await
        .expect("page list heading renders");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /");
}
