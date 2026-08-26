//! Integration tests for the invoice example (issue #1317).
//!
//! Covers the issue's worked-example acceptance criterion: a route that
//! returns a downloadable PDF rendered from a Maud template, verified via the
//! in-process test client (no headless browser, no Docker).

use autumn_web::routes;
use autumn_web::test::TestApp;
use autumn_web::time::FixedClock;
use chrono::{TimeZone, Utc};

fn client() -> autumn_web::test::TestClient {
    let fixed = Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap();
    // `routes!` needs the fully-qualified path (not a `use`-imported bare
    // name) so it can resolve each handler's hidden
    // `__autumn_route_info_*` companion function relative to the same path.
    TestApp::new()
        .routes(routes![invoice::invoice_detail, invoice::invoice_pdf])
        .with_clock(FixedClock::at(fixed))
        .build()
}

#[tokio::test]
async fn detail_page_renders_html() {
    client()
        .get("/invoices/42")
        .send()
        .await
        .assert_ok()
        .assert_body_contains("Invoice #42")
        .assert_body_contains("Total: $42.00");
}

#[tokio::test]
async fn pdf_route_sets_pdf_headers_and_filename() {
    client()
        .get("/invoices/42/pdf")
        .send()
        .await
        .assert_ok()
        .assert_header("content-type", "application/pdf")
        .assert_header(
            "content-disposition",
            "attachment; filename=\"invoice-42.pdf\"",
        );
}

#[tokio::test]
async fn pdf_route_renders_the_same_content_as_the_html_view() {
    client()
        .get("/invoices/42/pdf")
        .send()
        .await
        .assert_ok()
        .assert_pdf_contains("Invoice #42")
        .assert_pdf_contains("Widget")
        .assert_pdf_contains("Total: $42.00");
}

#[tokio::test]
async fn pdf_rendering_is_deterministic_given_a_fixed_clock() {
    let a = client().get("/invoices/7/pdf").send().await;
    let b = client().get("/invoices/7/pdf").send().await;
    assert_eq!(
        autumn_web::pdf::extract_text(&a.body).unwrap(),
        autumn_web::pdf::extract_text(&b.body).unwrap(),
        "identical model + fixed Clock must render identical text"
    );
}
