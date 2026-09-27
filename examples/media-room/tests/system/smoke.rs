//! Baseline Chromium smoke for the media-room example.
//!
//! Spawns the real `media-room` binary on an ephemeral port, drives a
//! headless-Chromium browser against its home page, and asserts expected
//! content with no uncaught console errors.
//!
//! Run (requires Chromium):
//!   cargo test -p media-room --features system-tests --test smoke -- --include-ignored
//!
//!   AUTUMN_CHROMIUM=/path/to/chrome cargo test ...   # custom binary

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn media_room_boots_and_serves_home_page() {
    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_media-room"),
        env!("CARGO_MANIFEST_DIR"),
        &[],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn media-room example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/").await.expect("visit /");
    page.expect_text("Autumn Media Rooms")
        .await
        .expect("home page renders its heading");
    page.expect_text("No rooms yet — create one above.")
        .await
        .expect("empty room list renders before any room is created");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on /");
}
