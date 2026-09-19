//! Integration tests for the [`Pdf`](autumn_web::pdf::Pdf) `IntoResponse`
//! (issue #1317).
//!
//! Covers the `Content-Type`/`Content-Disposition` header contract (built on
//! `Download`, including its RFC 6266 filename encoding), the supported HTML
//! subset (headings, tables, transparent wrapper tags), rendering through a
//! real `maud::Markup` view, and the determinism contract that backs
//! `TestResponse::assert_pdf_contains`.

use autumn_web::pdf::{Pdf, extract_text};
use axum::response::IntoResponse;
use http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};

fn text(pdf: &Pdf) -> String {
    extract_text(&pdf.render()).expect("valid PDF")
}

#[test]
fn renders_plain_paragraph_text() {
    let pdf = Pdf::from_html("<p>Total: $42.00</p>");
    assert!(text(&pdf).contains("Total: $42.00"));
}

#[test]
fn renders_heading_and_table_content() {
    let pdf = Pdf::from_html(
        "<h1>Invoice #42</h1><table><tr><th>Item</th><th>Amount</th></tr><tr><td>Widget</td><td>$42.00</td></tr></table>",
    );
    let extracted = text(&pdf);
    assert!(extracted.contains("Invoice"));
    assert!(extracted.contains("42"));
    assert!(extracted.contains("Widget"));
    assert!(extracted.contains("42.00"));
}

#[test]
fn unknown_wrapper_tags_still_render_their_text() {
    let pdf = Pdf::from_html(r#"<div class="card"><span>Total: $42.00</span></div>"#);
    assert!(text(&pdf).contains("Total: $42.00"));
}

#[test]
fn rendering_is_deterministic_for_identical_input() {
    let html = "<h1>Invoice</h1><p>Total: $42.00</p>";
    let a = extract_text(&Pdf::from_html(html).render()).expect("valid PDF");
    let b = extract_text(&Pdf::from_html(html).render()).expect("valid PDF");
    assert_eq!(a, b);
}

#[test]
fn default_response_is_attachment_pdf_named_document() {
    let resp = Pdf::from_html("<p>hi</p>").into_response();
    assert_eq!(resp.headers().get(CONTENT_TYPE).unwrap(), "application/pdf");
    assert_eq!(
        resp.headers().get(CONTENT_DISPOSITION).unwrap(),
        "attachment; filename=\"document.pdf\""
    );
}

#[test]
fn filename_and_inline_builders_are_honored() {
    let resp = Pdf::from_html("<p>hi</p>")
        .filename("invoice.pdf")
        .inline()
        .into_response();
    assert_eq!(
        resp.headers().get(CONTENT_DISPOSITION).unwrap(),
        "inline; filename=\"invoice.pdf\""
    );
}

#[test]
fn non_ascii_filename_gets_rfc_6266_extended_param() {
    // Proves `Pdf::filename` actually reaches `Download`'s RFC 6266 encoding
    // (non-ASCII/spaces), not just that it stores a string.
    let resp = Pdf::from_html("<p>hi</p>")
        .filename("facture éà 2026.pdf")
        .into_response();
    let disp = resp
        .headers()
        .get(CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(disp.is_ascii(), "header value must stay ASCII: {disp}");
    assert!(
        disp.contains("filename*=UTF-8''"),
        "expected an RFC 5987/6266 extended filename* parameter: {disp}"
    );
}

#[test]
fn output_starts_with_the_pdf_magic_bytes() {
    let bytes = Pdf::from_html("<p>hi</p>").render();
    assert!(bytes.starts_with(b"%PDF-"));
}

#[cfg(feature = "maud")]
#[test]
fn from_markup_renders_maud_views() {
    let markup = maud::html! {
        h1 { "Invoice" }
        p { "Total: $42.00" }
    };
    let pdf = Pdf::from_markup(markup);
    assert!(text(&pdf).contains("Total: $42.00"));
}

#[test]
fn table_row_taller_than_a_page_does_not_corrupt_later_columns() {
    // Regression: a table row's per-column rendering used to let a page
    // break happen mid-column (via `ensure_space` inside the shared
    // line-drawing helper), after which the *next* column resumed at a
    // stale y-coordinate captured before the break — from a page that had
    // already been flushed. Build a row where the first cell needs far more
    // vertical space than a whole page, and check the second cell's text
    // still renders and isn't dropped, and rendering doesn't panic.
    let long_cell = "word ".repeat(2000);
    let html = format!("<table><tr><td>{long_cell}</td><td>Amount: $42.00</td></tr></table>");
    let pdf = Pdf::from_html(html);
    let extracted = text(&pdf);
    assert!(
        extracted.contains("Amount: $42.00"),
        "second column's text must still render even though the first \
         column overflowed a page"
    );
}

#[test]
fn extract_text_rejects_non_pdf_bytes() {
    assert!(extract_text(b"not a pdf").is_err());
}
