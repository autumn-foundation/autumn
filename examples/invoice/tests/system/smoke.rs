//! Baseline Chromium smoke — issue #1317.
//!
//! Spawns the real `invoice` binary on an ephemeral port and drives a
//! headless-Chromium browser against the on-screen detail page. The PDF
//! export route (`/invoices/{id}/pdf`) is a download, not a navigable page —
//! it's covered by the in-process assertions in `tests/invoice.rs` instead,
//! which can inspect the response headers and extracted PDF text directly.
//!
//! Run (requires Chromium):
//!   cargo test -p invoice --features system-tests --test smoke -- --include-ignored
//!
//!   AUTUMN_CHROMIUM=/path/to/chrome cargo test ...   # custom binary

#![cfg(feature = "system-tests")]

#[tokio::test]
#[ignore = "requires Chromium — set AUTUMN_CHROMIUM or install chromium-browser"]
async fn invoice_boots_and_serves_the_detail_page() {
    let app = example_e2e::spawn_example(
        env!("CARGO_BIN_EXE_invoice"),
        env!("CARGO_MANIFEST_DIR"),
        &[],
        example_e2e::DEFAULT_READY_TIMEOUT,
    )
    .await
    .expect("spawn invoice example — is it built?");

    let runner = app
        .attach_browser()
        .await
        .expect("attach browser — is Chromium installed?");
    let page = runner.page().await.expect("open page");

    page.visit("/invoices/42")
        .await
        .expect("visit /invoices/42");
    page.expect_text("Invoice #42")
        .await
        .expect("detail page renders the invoice heading");
    page.expect_text("Widget")
        .await
        .expect("detail page renders line items");
    page.expect_no_console_errors()
        .await
        .expect("no console errors on the detail page");
}
