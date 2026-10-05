//! Chromium smoke for the `escape-hatches` example.
//!
//! Boots the real binary on a fresh Postgres (it migrates on boot in
//! `development`). A browser opens the product list and a missing SKU.
//!
//! Needs Chromium and Docker:
//!   cargo test -p escape-hatches --features system-tests --test smoke -- --include-ignored

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium + Docker — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn product_list_and_not_found_page_render() {
    let db = example_e2e::provision_postgres(1).await;
    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_escape-hatches"),
        env!("CARGO_MANIFEST_DIR"),
        &[("AUTUMN_DATABASE__URL", &db.urls()[0])],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn the escape-hatches example");

    let runner = app.attach_browser().await.expect("attach browser");
    let page = runner.page().await.expect("open page");

    page.visit("/").await.expect("visit /");
    page.expect_text("Stockroom")
        .await
        .expect("product list renders");
    page.expect_no_console_errors()
        .await
        .expect("no console errors");

    page.visit("/products/ZZ-9")
        .await
        .expect("visit a missing SKU");
    page.expect_text("ZZ-9")
        .await
        .expect("the 404 page names the SKU");
    page.expect_no_console_errors()
        .await
        .expect("no console errors");
}
